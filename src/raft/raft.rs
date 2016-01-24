
#![allow(dead_code)]
use std::cmp;
use raft::storage::Storage;
use util::SyncCell;
use proto::raftpb::{HardState, Entry, EntryType, Message, MessageType};
use protobuf::repeated::RepeatedField;
use raft::progress::{Progress, Inflights, ProgressState};
use raft::errors::{Result, Error, other, StorageError};
use std::collections::HashMap;
use raft::raft_log::{self, RaftLog};
use std::sync::Arc;


#[derive(Debug, PartialEq, Clone, Copy)]
enum StateRole {
    Follower,
    Candidate,
    Leader,
    Invalid,
}

impl Default for StateRole {
    fn default() -> StateRole {
        StateRole::Invalid
    }
}

const INVALID_ID: u64 = 0;

/// Config contains the parameters to start a raft.
#[derive(Default)]
pub struct Config<T: Storage + Sync> {
    /// id is the identity of the local raft. ID cannot be 0.
    id: u64,

    /// peers contains the IDs of all nodes (including self) in
    /// the raft cluster. It should only be set when starting a new
    /// raft cluster.
    /// Restarting raft from previous configuration will panic if
    /// peers is set.
    /// peer is private and only used for testing right now.
    peers: Vec<u64>,

    /// ElectionTick is the election timeout. If a follower does not
    /// receive any message from the leader of current term during
    /// ElectionTick, it will become candidate and start an election.
    /// ElectionTick must be greater than HeartbeatTick. We suggest
    /// to use ElectionTick = 10 * HeartbeatTick to avoid unnecessary
    /// leader switching.
    election_tick: usize,
    /// HeartbeatTick is the heartbeat usizeerval. A leader sends heartbeat
    /// message to mausizeain the leadership every heartbeat usizeerval.
    heartbeat_tick: usize,

    /// Storage is the storage for raft. raft generates entires and
    /// states to be stored in storage. raft reads the persisted entires
    /// and states out of Storage when it needs. raft reads out the previous
    /// state and configuration out of storage when restarting.
    storage: Option<Arc<SyncCell<T>>>,
    /// Applied is the last applied index. It should only be set when restarting
    /// raft. raft will not return entries to the application smaller or equal to Applied.
    /// If Applied is unset when restarting, raft might return previous applied entries.
    /// This is a very application dependent configuration.
    applied: u64,

    /// MaxSizePerMsg limits the max size of each append message. Smaller value lowers
    /// the raft recovery cost(initial probing and message lost during normal operation).
    /// On the other side, it might affect the throughput during normal replication.
    /// Note: math.MaxUusize64 for unlimited, 0 for at most one entry per message.
    max_size_per_msg: u64,
    /// max_inflight_msgs limits the max number of in-flight append messages during optimistic
    /// replication phase. The application transportation layer usually has its own sending
    /// buffer over TCP/UDP. Setting MaxInflightMsgs to avoid overflowing that sending buffer.
    /// TODO (xiangli): feedback to application to limit the proposal rate?
    max_inflight_msgs: usize,

    /// check_quorum specifies if the leader should check quorum activity. Leader steps down when
    /// quorum is not active for an electionTimeout.
    check_quorum: bool,
}

impl<T: Storage + Sync> Config<T> {
    pub fn validate(&self) -> Result<()> {
        if self.id == INVALID_ID {
            return Err(other("invalid node id"));
        }

        if self.heartbeat_tick <= 0 {
            return Err(other("heartbeat tick must greater than 0"));
        }

        if self.election_tick <= self.heartbeat_tick {
            return Err(other("election tick must be greater than heartbeat tick"));
        }

        if self.storage.is_none() {
            return Err(other("storage should be specified"));
        }

        if self.max_inflight_msgs <= 0 {
            return Err(other("max inflight messages must be greater than 0"));
        }

        Ok(())
    }
}

// SoftState provides state that is useful for logging and debugging.
// The state is volatile and does not need to be persisted to the WAL.
#[derive(Default, PartialEq)]
struct SoftState {
    lead: u64,
    raft_state: StateRole,
}

struct Raft<T: Storage + Sync> {
    hs: HardState,

    id: u64,

    /// the log
    raft_log: RaftLog<T>,

    max_inflight: usize,
    max_msg_size: u64,
    prs: HashMap<u64, Progress>,

    state: StateRole,

    votes: HashMap<u64, bool>,

    msgs: Vec<Message>,

    /// the leader id
    lead: u64,

    /// New configuration is ignored if there exists unapplied configuration.
    pending_conf: bool,

    /// number of ticks since it reached last electionTimeout when it is leader
    /// or candidate.
    /// number of ticks since it reached last electionTimeout or received a
    /// valid message from current leader when it is a follower.
    election_elapsed: usize,

    /// number of ticks since it reached last heartbeatTimeout.
    /// only leader keeps heartbeatElapsed.
    heartbeat_elapsed: usize,

    check_quorum: bool,

    heartbeat_timeout: usize,
    election_timeout: usize,
    tick_func: fn(&mut Raft<T>) -> (),
    step_func: fn(&mut Raft<T>, Message) -> (),
}

fn new_progress(next_idx: u64, ins_size: usize) -> Progress {
    Progress {
        next_idx: next_idx,
        ins: Inflights::new(ins_size),
        ..Default::default()
    }
}

fn new_message(from: u64, field_type: MessageType) -> Message {
    let mut m = Message::new();
    m.set_from(from);
    m.set_field_type(field_type);
    m
}

impl<T: Storage + Sync> Raft<T> {
    fn new(c: &Config<T>) -> Raft<T> {
        c.validate().expect("configuration is invalid");
        let store = c.storage.as_ref().unwrap().clone();
        let rs = store.initial_state().expect("");
        let raft_log = RaftLog::new(store);
        let mut peers: &[u64] = &c.peers;
        if rs.conf_state.get_nodes().len() > 0 {
            if peers.len() > 0 {
                // TODO(bdarnell): the peers argument is always nil except in
                // tests; the argument should be removed and these tests should be
                // updated to specify their nodes through a snap
                panic!("cannot specify both new(peers) and ConfState.Nodes")
            }
            peers = rs.conf_state.get_nodes();
        }
        let mut r = Raft {
            hs: HardState::new(),
            id: c.id,
            raft_log: raft_log,
            max_inflight: c.max_inflight_msgs,
            max_msg_size: c.max_size_per_msg,
            prs: HashMap::with_capacity(peers.len()),
            state: StateRole::Invalid,
            votes: HashMap::new(),
            msgs: vec![],
            lead: INVALID_ID,
            pending_conf: false,
            tick_func: Raft::tick_election,
            step_func: Raft::step_follower,
            check_quorum: c.check_quorum,
            heartbeat_elapsed: 0,
            election_elapsed: 0,
            heartbeat_timeout: c.heartbeat_tick,
            election_timeout: c.election_tick,
        };
        for p in peers {
            r.prs.insert(*p, new_progress(1, r.max_inflight));
        }
        if rs.hard_state != HardState::new() {
            r.load_state(rs.hard_state);
        }
        if c.applied > 0 {
            r.raft_log.applied_to(c.applied);
        }
        let term = r.hs.get_term();
        r.become_follower(term, INVALID_ID);

        let mut nodes_str = String::new();
        for n in r.nodes() {
            nodes_str = nodes_str + &format!("{:?}", n);
            nodes_str = nodes_str + ",";
        }
        let nodes_str_len = nodes_str.len();
        nodes_str.remove(nodes_str_len - 1);
        info!("newRaft {:x} [peers: [{}], term: {:?}, commit: {}, applied: {}, last_index: {}, \
               last_term: {}]",
              r.id,
              nodes_str,
              r.hs.get_term(),
              r.raft_log.get_committed(),
              r.raft_log.get_applied(),
              r.raft_log.last_index(),
              r.raft_log.last_term());
        r
    }

    fn has_leader(&self) -> bool {
        self.lead != INVALID_ID
    }

    fn soft_state(&self) -> SoftState {
        SoftState {
            lead: self.lead,
            raft_state: self.state,
        }
    }

    fn quorum(&self) -> usize {
        self.prs.len() / 2 + 1
    }

    fn nodes(&self) -> Vec<u64> {
        let mut nodes = Vec::with_capacity(self.prs.len());
        nodes.extend(self.prs.keys());
        nodes.sort();
        nodes
    }

    // send persists state to stable storage and then sends to its mailbox.
    fn send(&mut self, m: Message) {
        let mut m = m;
        m.set_from(self.id);
        // do not attach term to MsgPropose
        // proposals are a way to forward to the leader and
        // should be treated as local message.
        if m.get_field_type() != MessageType::MsgPropose {
            m.set_term(self.hs.get_term());
        }
        self.msgs.push(m);
    }

    fn prepare_send_snapshot(&mut self, m: &mut Message, to: u64) {
        let pr = self.prs.get_mut(&to).unwrap();
        if !pr.recent_active {
            debug!("ignore sending snapshot to {:x} since it is not recently active",
                   to);
            return;
        }

        m.set_field_type(MessageType::MsgSnapshot);
        let snapshot_r = self.raft_log.snapshot();
        if let Err(e) = snapshot_r {
            if e == Error::Store(StorageError::SnapshotTemporarilyUnavailable) {
                debug!("{:x} failed to send snapshot to {:x} because snapshot is termporarily \
                        unavailable",
                       self.id,
                       to);
                return;
            }
            panic!(e);
        }
        let snapshot = snapshot_r.unwrap();
        if snapshot.get_metadata().get_index() == 0 {
            panic!("need non-empty snapshot");
        }
        let (sindex, sterm) = (snapshot.get_metadata().get_index(),
                               snapshot.get_metadata().get_term());
        m.set_snapshot(snapshot.clone());
        debug!("{:x} [firstindex: {}, commit: {}] sent snapshot[index: {}, term: {}] to {:x} \
                [{:?}]",
               self.id,
               self.raft_log.first_index(),
               self.hs.get_commit(),
               sindex,
               sterm,
               to,
               pr);
        pr.become_snapshot(sindex);
        debug!("{:x} paused sending replication messages to {:x} [{:?}]",
               self.id,
               to,
               pr);
    }

    fn prepare_send_entries(&mut self, m: &mut Message, to: u64, term: u64, ents: Vec<Entry>) {
        let pr = self.prs.get_mut(&to).unwrap();
        m.set_field_type(MessageType::MsgAppend);
        m.set_index(pr.next_idx - 1);
        m.set_logTerm(term);
        m.set_entries(RepeatedField::from_vec(ents));
        m.set_commit(self.raft_log.get_committed());
        if m.get_entries().len() != 0 {
            match pr.state {
                ProgressState::Replicate => {
                    let last = m.get_entries().last().unwrap().get_Index();
                    pr.optimistic_update(last);
                    pr.ins.add(last);
                }
                ProgressState::Probe => pr.pause(),
                _ => {
                    panic!("{:x} is sending append in unhandled state {:?}",
                           self.id,
                           pr.state)
                }
            }
        }
    }

    // send_append sends RPC, with entries to the given peer.
    fn send_append(&mut self, to: u64) {
        let (term, ents) = {
            let pr = self.prs.get(&to).unwrap();
            if pr.is_paused() {
                return;
            }
            (self.raft_log.term(pr.next_idx - 1),
             self.raft_log.entries(pr.next_idx, self.max_msg_size))
        };
        let mut m = Message::new();
        m.set_to(to);
        if term.is_err() || ents.is_err() {
            // send snapshot if we failed to get term or entries
            self.prepare_send_snapshot(&mut m, to);
        } else {
            self.prepare_send_entries(&mut m, to, term.unwrap(), ents.unwrap());
        }
        self.send(m);
    }

    // sendHeartbeat sends an empty MsgApp
    fn send_heartbeat(&mut self, to: u64) {
        // Attach the commit as min(to.matched, r.committed).
        // When the leader sends out heartbeat message,
        // the receiver(follower) might not be matched with the leader
        // or it might not have all the committed entries.
        // The leader MUST NOT forward the follower's commit to
        // an unmatched index.
        let commit = cmp::min(self.prs.get(&to).unwrap().matched,
                              self.raft_log.get_committed());
        let mut m = Message::new();
        m.set_to(to);
        m.set_field_type(MessageType::MsgHeartbeat);
        m.set_commit(commit);
        self.send(m);
    }

    // bcastAppend sends RPC, with entries to all peers that are not up-to-date
    // according to the progress recorded in r.prs.
    fn bcast_append(&mut self) {
        let keys: Vec<u64> = self.prs.keys().map(|x| *x).collect();
        for id in keys {
            if id == self.id {
                continue;
            }
            self.send_append(id);
        }
    }

    // bcastHeartbeat sends RPC, without entries to all the peers.
    fn bcast_heartbeat(&mut self) {
        let keys: Vec<u64> = self.prs.keys().map(|x| *x).collect();
        for id in keys {
            if id == self.id {
                continue;
            }
            self.send_heartbeat(id);
            self.prs.get_mut(&id).unwrap().resume()
        }
    }

    fn maybe_commit(&mut self) -> bool {
        // TODO: optimize
        let mut mis = Vec::with_capacity(self.prs.len());
        let keys: Vec<u64> = self.prs.keys().map(|x| *x).collect();
        for id in keys {
            mis.push(self.prs[&id].matched);
        }
        mis.sort_by(|a, b| b.cmp(a));
        let mci = mis[self.quorum() - 1];
        self.raft_log.maybe_commit(mci, self.hs.get_term())
    }

    fn reset(&mut self, term: u64) {
        if self.hs.get_term() != term {
            self.hs.set_term(term);
            self.hs.set_vote(INVALID_ID);
        }
        self.lead = INVALID_ID;
        self.election_elapsed = 0;
        self.heartbeat_elapsed = 0;

        self.votes = HashMap::new();
        let keys: Vec<u64> = self.prs.keys().map(|x| *x).collect();
        for id in keys {
            *self.prs.get_mut(&id).unwrap() = new_progress(self.raft_log.last_index() + 1,
                                                           self.max_inflight);
            if id == self.id {
                self.prs.get_mut(&id).unwrap().matched = self.raft_log.last_index();
            }
        }
        self.pending_conf = false;
    }

    fn append_entry(&mut self, es: &mut [Entry]) {
        let li = self.raft_log.last_index();
        for i in 0..es.len() {
            let e = es.get_mut(i).unwrap();
            e.set_Term(self.hs.get_term());
            e.set_Index(li + 1 + i as u64);
        }
        self.raft_log.append(es);
        let id = self.id;
        let last_index = self.raft_log.last_index();
        self.prs.get_mut(&id).unwrap().maybe_update(last_index);
        self.maybe_commit();
    }

    // tickElection is run by followers and candidates after self.election_timeout.
    fn tick_election(&mut self) {
        if !self.promotable() {
            self.election_elapsed = 0;
            return;
        }
        self.election_elapsed += 1;
        if self.is_election_timeout() {
            self.election_elapsed = 0;
            let m = new_message(self.id, MessageType::MsgHup);
            self.step(m);
        }
    }

    // tickHeartbeat is run by leaders to send a MsgBeat after r.heartbeatTimeout.
    fn tick_heartbeat(&mut self) {
        self.heartbeat_elapsed += 1;
        self.election_elapsed += 1;

        if self.election_elapsed >= self.election_timeout {
            self.election_elapsed = 0;
            if self.check_quorum {
                let m = new_message(self.id, MessageType::MsgCheckQuorum);
                self.step(m);
            }
        }

        if self.state != StateRole::Leader {
            return;
        }

        if self.heartbeat_elapsed >= self.heartbeat_timeout {
            self.heartbeat_elapsed = 0;
            let m = new_message(self.id, MessageType::MsgBeat);
            self.step(m);
        }
    }

    fn become_follower(&mut self, term: u64, lead: u64) {
        self.step_func = Raft::step_follower;
        self.reset(term);
        self.tick_func = Raft::tick_election;
        self.lead = lead;
        self.state = StateRole::Follower;
        info!("{:x} became follower at term {}",
              self.id,
              self.hs.get_term());
    }

    fn become_candidate(&mut self) {
        assert!(self.state != StateRole::Leader,
                "invalid transition [leader -> candidate]");
        self.step_func = Raft::step_candidate;
        let term = self.hs.get_term() + 1;
        self.reset(term);
        self.tick_func = Raft::tick_election;
        let id = self.id;
        self.hs.set_vote(id);
        self.state = StateRole::Candidate;
        info!("{:x} became candidate at term {}",
              self.id,
              self.hs.get_term());
    }

    fn become_leader(&mut self) {
        assert!(self.state != StateRole::Follower,
                "invalid transition [follower -> leader]");
        self.step_func = Raft::step_leader;
        let term = self.hs.get_term();
        self.reset(term);
        self.tick_func = Raft::tick_heartbeat;
        self.lead = self.id;
        self.state = StateRole::Leader;
        let begin = self.raft_log.get_committed() + 1;
        let ents = self.raft_log
                       .entries(begin, raft_log::NO_LIMIT)
                       .expect("unexpected error getting uncommitted entries");
        for e in ents {
            if e.get_Type() != EntryType::EntryConfChange {
                continue;
            }
            assert!(!self.pending_conf,
                    "unexpected double uncommitted config entry");
            self.pending_conf = true;
        }
        self.append_entry(&mut [Entry::new()]);
        info!("{:x} became leader at term {}", self.id, self.hs.get_term());
    }

    fn compaign(&mut self) {
        self.become_candidate();
        let id = self.id;
        let poll_res = self.poll(id, true);
        if self.quorum() == poll_res {
            self.become_leader();
            return;
        }
        let keys: Vec<u64> = self.prs.keys().map(|x| *x).collect();
        for id in keys {
            if id == self.id {
                continue;
            }
            info!("{:x} [logterm: {}, index: {}] sent vote request to {:x} at term {}",
                  self.id,
                  self.raft_log.last_term(),
                  self.raft_log.last_index(),
                  id,
                  self.hs.get_term());
            let mut m = Message::new();
            m.set_to(id);
            m.set_field_type(MessageType::MsgRequestVote);
            m.set_index(self.raft_log.last_index());
            m.set_logTerm(self.raft_log.last_term());
            self.send(m);
        }
    }

    fn get_term(&self) -> u64 {
        self.hs.get_term()
    }

    fn poll(&mut self, id: u64, v: bool) -> usize {
        if v {
            info!("{:x} received vote from {:x} at term {}",
                  self.id,
                  id,
                  self.get_term())
        } else {
            info!("{:x} received vote rejection from {:x} at term {}",
                  self.id,
                  id,
                  self.get_term())
        }
        if !self.votes[&id] {
            self.votes.insert(id, v);
        }
        self.votes.values().filter(|x| **x).count()
    }

    fn step(&mut self, m: Message) {
        if m.get_field_type() == MessageType::MsgHup {
            if self.state != StateRole::Leader {
                info!("{:x} is starting a new election at term {}",
                      self.id,
                      self.get_term());
                self.compaign();
                let committed = self.raft_log.get_committed();
                self.hs.set_commit(committed);
            } else {
                debug!("{:x} ignoring MsgHup because already leader", self.id);
            }
            return;
        }

        if m.get_term() == 0 {
            // local message
        } else if m.get_term() > self.get_term() {
            let mut lead = m.get_from();
            if m.get_field_type() == MessageType::MsgRequestVote {
                lead = INVALID_ID;
            }
            info!("{:x} [term: {}] received a {:?} message with higher term from {:x} [term: {}]",
                  self.id,
                  self.get_term(),
                  m.get_field_type(),
                  m.get_from(),
                  m.get_term());
            self.become_follower(m.get_term(), lead);
        } else {
            // ignore
            info!("{:x} [term: {}] ignored a {:?} message with lower term from {} [term: {}]",
                  self.id,
                  self.get_term(),
                  m.get_field_type(),
                  m.get_from(),
                  m.get_term());
            return;
        }
        (self.step_func)(self, m);
        let committed = self.raft_log.get_committed();
        self.hs.set_commit(committed);
    }

    fn step_leader(&mut self, m: Message) {
        // These message types do not require any progress for m.From.
        match m.get_field_type() {
            MessageType::MsgBeat => {
                self.bcast_heartbeat();
                return;
            }
            MessageType::MsgCheckQuorum => {
                if !self.check_quorum_active() {
                    warn!("{:x} stepped down to follower since quorum is not active",
                          self.id);
                    let term = self.get_term();
                    self.become_follower(term, INVALID_ID);
                }
                return;
            }
            MessageType::MsgPropose => {
                if m.get_entries().len() == 0 {
                    panic!("{:x} stepped empty MsgProp", self.id);
                }
                let id = self.id;
                if self.prs.get(&id).is_none() {
                    // If we are not currently a member of the range (i.e. this node
                    // was removed from the configuration while serving as leader),
                    // drop any new proposals.
                    return;
                }
                let mut m = m;
                if self.pending_conf {
                    for e in m.mut_entries().iter_mut() {
                        if e.get_Type() == EntryType::EntryConfChange {
                            *e = Entry::new();
                            e.set_Type(EntryType::EntryNormal);
                        }
                    }
                }
                self.append_entry(&mut m.mut_entries());
                self.bcast_append();
                return;
            }
            MessageType::MsgRequestVote => {
                info!("{:x} [logterm: {}, index: {}, vote: {:x}] rejected vote from {:x} \
                       [logterm: {}, index: {}] at term {}",
                      self.id,
                      self.raft_log.last_term(),
                      self.raft_log.last_index(),
                      self.hs.get_vote(),
                      m.get_from(),
                      m.get_logTerm(),
                      m.get_index(),
                      self.get_term());
                let mut to_sent_m = Message::new();
                to_sent_m.set_to(m.get_to());
                to_sent_m.set_field_type(MessageType::MsgRequestVoteResponse);
                to_sent_m.set_reject(true);
                self.send(to_sent_m);
            }
            _ => {}
        }

        // All other message types require a progress for m.get_from (pr).
        let mut send_append = false;
        let mut maybe_commit = false;
        let mut old_paused = false;
        {
            let pr = self.prs.get_mut(&m.get_from());
            if pr.is_none() {
                debug!("no progress available for {:x}", m.get_from());
                return;
            }
            let pr = pr.unwrap();
            match m.get_field_type() {
                MessageType::MsgAppendResponse => {
                    pr.recent_active = true;
                    if m.get_reject() {
                        debug!("{:x} received msgApp rejection(lastindex: {}) from {:x} for \
                                index {}",
                               self.id,
                               m.get_rejectHint(),
                               m.get_from(),
                               m.get_index());
                        if pr.maybe_decr_to(m.get_index(), m.get_rejectHint()) {
                            debug!("{:x} decreased progress of {:x} to [{:?}]",
                                   self.id,
                                   m.get_from(),
                                   pr);
                            if pr.state == ProgressState::Replicate {
                                pr.become_probe();
                            }
                            send_append = true;
                        }
                    } else {
                        old_paused = pr.is_paused();
                        if pr.maybe_update(m.get_index()) {
                            match pr.state {
                                ProgressState::Probe => pr.become_replicate(),
                                ProgressState::Snapshot if pr.maybe_snapshot_abort() => {
                                    debug!("{:x} snapshot aborted, resumed sending replication \
                                            messages to {:x} [{:?}]",
                                           self.id,
                                           m.get_from(),
                                           pr);
                                    pr.become_probe();
                                }
                                ProgressState::Replicate => pr.ins.free_to(m.get_index()),
                                _ => {}
                            }
                            maybe_commit = true;
                        }
                    }
                }
                MessageType::MsgHeartbeatResponse => {
                    pr.recent_active = true;

                    // free one slot for the full inflights window to allow progress.
                    if pr.state == ProgressState::Replicate && pr.ins.full() {
                        pr.ins.free_first_one();
                    }
                    if pr.matched < self.raft_log.last_index() {
                        send_append = true;
                    }
                }
                MessageType::MsgSnapStatus => {
                    if pr.state != ProgressState::Snapshot {
                        return;
                    }
                    if !m.get_reject() {
                        pr.become_probe();
                        debug!("{:x} snapshot succeeded, resumed sending replication messages to \
                                {:x} [{:?}]",
                               self.id,
                               m.get_from(),
                               pr);
                    } else {
                        pr.snapshot_failure();
                        pr.become_probe();
                        debug!("{:x} snapshot failed, resumed sending replication messages to \
                                {:x} [{:?}]",
                               self.id,
                               m.get_from(),
                               pr);
                    }
                    // If snapshot finish, wait for the msgAppResp from the remote node before sending
                    // out the next msgApp.
                    // If snapshot failure, wait for a heartbeat interval before next try
                    pr.pause()
                }
                MessageType::MsgUnreachable => {
                    // During optimistic replication, if the remote becomes unreachable,
                    // there is huge probability that a MsgApp is lost.
                    if pr.state == ProgressState::Replicate {
                        pr.become_probe();
                    }
                    debug!("{:x} failed to send message to {:x} because it is unreachable [{:?}]",
                           self.id,
                           m.get_from(),
                           pr);
                }
                _ => {}
            }
        }
        if maybe_commit {
            if self.maybe_commit() {
                self.bcast_append();
            } else if old_paused {
                // update() reset the wait state on this node. If we had delayed sending
                // an update before, send it now.
                send_append = true;
            }
        }
        if send_append {
            self.send_append(m.get_from());
        }
    }

    fn step_follower(&mut self, m: Message) {}
    fn check_quorum_active(&self) -> bool {
        true
    }
    fn step_candidate(&mut self, m: Message) {}
    fn promotable(&self) -> bool {
        true
    }
    fn is_election_timeout(&self) -> bool {
        true
    }

    fn load_state(&self, hs: HardState) {}
}
