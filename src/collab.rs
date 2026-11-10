// Collaborative editing — end-to-end encrypted real-time co-editing.
//
// Algorithm: server-ordered operational transformation (the ot.js / Jupiter
// scheme). The host is the server: it owns every shared document, assigns each
// applied operation a revision number, and transforms an incoming operation
// against everything applied since the revision the guest based it on. A guest
// has at most one operation in flight per document; further local edits are
// composed into a buffer until the host acknowledges it. `TextOperation`
// (retain / insert / delete spans covering the whole document) gives `compose`
// and `transform` that provably converge, unlike position-pair transforms.
//
// Documents are identified by their workspace-relative path ("src/main.rs") on
// both sides; the host maps that to its file, a guest to a tab with
// `collab_path` set.
//
// Encryption: XChaCha20-Poly1305 with a pre-shared session key (PSK).
// The host generates a random 32-byte key and embeds it in the invite string:
//   lt-collab://IP:PORT#base64url(key)
// Each message is padded to a multiple of 512 bytes, then encrypted with a
// fresh random 24-byte nonce.  The Poly1305 tag provides authentication.
//
// Replay protection: on accept the host sends `HELLO_MAGIC` + a random 32-byte
// connection id in the clear. Every encrypted frame after that (the guest's Join
// first) carries (magic, connection id, direction, sequence number) as AEAD
// associated data, so a frame only decrypts at its own position, in its own
// direction, on its own connection (see `FrameCipher`).
//
// Wire format per frame (over raw TCP):
//   [4-byte big-endian u32 frame_len][24-byte nonce][ciphertext+tag]
// frame_len = 24 + len(ciphertext+tag)
//
// Background threads for all network I/O; results delivered to the main loop
// via EventLoopProxy<UserEvent>, following the SSH/LSP pattern.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rand::RngCore;
use ropey::Rope;
use winit::event_loop::EventLoopProxy;

use crate::UserEvent;

// ── Role system ───────────────────────────────────────────────────────────────

/// Five-tier peer role (ascending permissions). Host is implicit (not stored).
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerRole { Viewer, Contributor, SystemAccess, Moderator }

impl PeerRole {
    pub fn label(self) -> &'static str {
        match self {
            Self::Viewer       => "Viewer",
            Self::Contributor  => "Contributor",
            Self::SystemAccess => "System Access",
            Self::Moderator    => "Moderator",
        }
    }
    pub fn all() -> &'static [PeerRole] {
        &[Self::Viewer, Self::Contributor, Self::SystemAccess, Self::Moderator]
    }
}

/// Permission flags stored as a u16 bitfield.
pub mod perms {
    pub const VIEW_FILES:     u16 = 0b0000_0001;
    pub const WRITE_FILES:    u16 = 0b0000_0010;
    pub const VIEW_HIDDEN:    u16 = 0b0000_0100;
    pub const WRITE_HIDDEN:   u16 = 0b0000_1000;
    pub const VIEW_TERMINALS: u16 = 0b0001_0000;
    pub const OPEN_TERMINALS: u16 = 0b0010_0000;
    pub const MANAGE_ROLES:   u16 = 0b0100_0000;
    pub const ALL: u16            = 0b0111_1111;
}

/// Host-configurable permission set for each non-host role.
/// Persisted in settings.json so defaults survive session restarts.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RolePermissions {
    pub viewer:        u16,
    pub contributor:   u16,
    pub system_access: u16,
    pub moderator:     u16,
    pub default_role:  PeerRole,
}

impl Default for RolePermissions {
    fn default() -> Self {
        RolePermissions {
            viewer:        perms::VIEW_FILES,
            contributor:   perms::VIEW_FILES | perms::WRITE_FILES,
            system_access: perms::VIEW_FILES | perms::WRITE_FILES
                         | perms::VIEW_HIDDEN | perms::WRITE_HIDDEN
                         | perms::VIEW_TERMINALS | perms::OPEN_TERMINALS,
            moderator:     perms::VIEW_FILES | perms::WRITE_FILES
                         | perms::VIEW_HIDDEN | perms::WRITE_HIDDEN
                         | perms::VIEW_TERMINALS | perms::OPEN_TERMINALS
                         | perms::MANAGE_ROLES,
            default_role:  PeerRole::Viewer,
        }
    }
}

impl RolePermissions {
    pub fn for_role(&self, role: PeerRole) -> u16 {
        match role {
            PeerRole::Viewer       => self.viewer,
            PeerRole::Contributor  => self.contributor,
            PeerRole::SystemAccess => self.system_access,
            PeerRole::Moderator    => self.moderator,
        }
    }
    pub fn set_for_role(&mut self, role: PeerRole, bits: u16) {
        match role {
            PeerRole::Viewer       => self.viewer        = bits,
            PeerRole::Contributor  => self.contributor   = bits,
            PeerRole::SystemAccess => self.system_access = bits,
            PeerRole::Moderator    => self.moderator      = bits,
        }
    }
}

use std::collections::{HashSet, VecDeque};
use std::net::Shutdown;
use std::sync::atomic::{AtomicUsize, Ordering};

// ── Peer colours (assigned round-robin by host) ───────────────────────────────
const PEER_COLORS: &[u32] = &[
    0xFF6B6B, // coral red
    0x6BCB77, // green
    0x4D96FF, // blue
    0xFFD93D, // yellow
    0xC77DFF, // purple
    0xFF9A3C, // orange
    0x00C2C7, // teal
    0xF72585, // pink
];

// ── TextOperation (port of ot.js) ─────────────────────────────────────────────
//
// An operation walks the whole document: Retain(n) keeps n chars, Insert(s)
// inserts s, Delete(n) removes n chars. `base_len` is the document length (in
// chars — Unicode scalar values, the same unit as ropey) it applies to, and
// `target_len` the length afterwards. Normal form: adjacent components of the
// same kind are merged and an insert always precedes an adjacent delete.

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Comp {
    R(usize),
    I(String),
    D(usize),
}

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TextOperation {
    pub ops: Vec<Comp>,
    pub base_len: usize,
    pub target_len: usize,
}

fn char_len(s: &str) -> usize { s.chars().count() }

/// Split `s` after `n` chars.
fn split_at_char(s: &str, n: usize) -> (&str, &str) {
    match s.char_indices().nth(n) {
        Some((b, _)) => s.split_at(b),
        None => (s, ""),
    }
}

impl TextOperation {
    pub fn new() -> Self { Self::default() }

    pub fn retain(&mut self, n: usize) -> &mut Self {
        if n == 0 { return self; }
        self.base_len += n;
        self.target_len += n;
        if let Some(Comp::R(last)) = self.ops.last_mut() { *last += n; } else { self.ops.push(Comp::R(n)); }
        self
    }

    pub fn insert(&mut self, s: &str) -> &mut Self {
        if s.is_empty() { return self; }
        self.target_len += char_len(s);
        let len = self.ops.len();
        match self.ops.last_mut() {
            Some(Comp::I(last)) => last.push_str(s),
            Some(Comp::D(_)) => {
                // Keep inserts before deletes: merge into a preceding insert or put
                // the insert in front of the delete.
                if len >= 2 {
                    if let Comp::I(prev) = &mut self.ops[len - 2] {
                        prev.push_str(s);
                        return self;
                    }
                }
                self.ops.insert(len - 1, Comp::I(s.to_owned()));
            }
            _ => self.ops.push(Comp::I(s.to_owned())),
        }
        self
    }

    pub fn delete(&mut self, n: usize) -> &mut Self {
        if n == 0 { return self; }
        self.base_len += n;
        if let Some(Comp::D(last)) = self.ops.last_mut() { *last += n; } else { self.ops.push(Comp::D(n)); }
        self
    }

    pub fn is_noop(&self) -> bool {
        self.ops.iter().all(|c| matches!(c, Comp::R(_)))
    }

    /// The operation turning a document of `base_len` chars into one where
    /// `deleted` chars at `prefix` were replaced by `inserted` (`suffix` chars kept
    /// after the change). This is what a local edit looks like after diffing.
    pub fn from_change(prefix: usize, deleted: usize, inserted: &str, suffix: usize) -> Self {
        let mut op = TextOperation::new();
        op.retain(prefix).insert(inserted).delete(deleted).retain(suffix);
        op
    }

    /// Apply to a rope. Fails (leaving the rope unchanged) if the lengths don't match.
    pub fn apply(&self, rope: &mut Rope) -> Result<(), String> {
        if rope.len_chars() != self.base_len {
            return Err(format!("operation base length {} != document length {}", self.base_len, rope.len_chars()));
        }
        let mut pos = 0;
        for c in &self.ops {
            match c {
                Comp::R(n) => pos += n,
                Comp::I(s) => { rope.insert(pos, s); pos += char_len(s); }
                Comp::D(n) => rope.remove(pos..pos + n),
            }
        }
        Ok(())
    }

    /// `self` followed by `b`, as one operation.
    pub fn compose(&self, b: &TextOperation) -> Result<TextOperation, String> {
        if self.target_len != b.base_len {
            return Err("compose: length mismatch".to_owned());
        }
        let mut out = TextOperation::new();
        let mut i1 = self.ops.iter().cloned();
        let mut i2 = b.ops.iter().cloned();
        let (mut op1, mut op2) = (i1.next(), i2.next());
        loop {
            match (op1.take(), op2.take()) {
                (None, None) => break,
                (Some(Comp::D(n)), o2) => { out.delete(n); op1 = i1.next(); op2 = o2; }
                (o1, Some(Comp::I(s))) => { out.insert(&s); op1 = o1; op2 = i2.next(); }
                (None, _) => return Err("compose: first operation too short".to_owned()),
                (_, None) => return Err("compose: first operation too long".to_owned()),
                (Some(Comp::R(a)), Some(Comp::R(b2))) => {
                    let m = a.min(b2);
                    out.retain(m);
                    op1 = if a > m { Some(Comp::R(a - m)) } else { i1.next() };
                    op2 = if b2 > m { Some(Comp::R(b2 - m)) } else { i2.next() };
                }
                (Some(Comp::I(s)), Some(Comp::D(d))) => {
                    let l = char_len(&s);
                    let m = l.min(d);
                    op1 = if l > m { Some(Comp::I(split_at_char(&s, m).1.to_owned())) } else { i1.next() };
                    op2 = if d > m { Some(Comp::D(d - m)) } else { i2.next() };
                }
                (Some(Comp::I(s)), Some(Comp::R(r))) => {
                    let l = char_len(&s);
                    let m = l.min(r);
                    let (head, tail) = split_at_char(&s, m);
                    out.insert(head);
                    op1 = if l > m { Some(Comp::I(tail.to_owned())) } else { i1.next() };
                    op2 = if r > m { Some(Comp::R(r - m)) } else { i2.next() };
                }
                (Some(Comp::R(r)), Some(Comp::D(d))) => {
                    let m = r.min(d);
                    out.delete(m);
                    op1 = if r > m { Some(Comp::R(r - m)) } else { i1.next() };
                    op2 = if d > m { Some(Comp::D(d - m)) } else { i2.next() };
                }
            }
        }
        Ok(out)
    }

    /// Transform two concurrent operations `a` and `b` (same base). Returns
    /// `(a', b')` such that `apply(apply(doc, a), b') == apply(apply(doc, b), a')`.
    /// When both insert at the same position, `a`'s text comes first; the server
    /// and clients always pass the guest's own operation as `a`.
    pub fn transform(a: &TextOperation, b: &TextOperation) -> Result<(TextOperation, TextOperation), String> {
        if a.base_len != b.base_len {
            return Err("transform: base length mismatch".to_owned());
        }
        let (mut ap, mut bp) = (TextOperation::new(), TextOperation::new());
        let mut i1 = a.ops.iter().cloned();
        let mut i2 = b.ops.iter().cloned();
        let (mut op1, mut op2) = (i1.next(), i2.next());
        loop {
            match (op1.take(), op2.take()) {
                (None, None) => break,
                (Some(Comp::I(s)), o2) => {
                    bp.retain(char_len(&s));
                    ap.insert(&s);
                    op1 = i1.next();
                    op2 = o2;
                }
                (o1, Some(Comp::I(s))) => {
                    ap.retain(char_len(&s));
                    bp.insert(&s);
                    op1 = o1;
                    op2 = i2.next();
                }
                (None, _) => return Err("transform: first operation too short".to_owned()),
                (_, None) => return Err("transform: first operation too long".to_owned()),
                (Some(Comp::R(x)), Some(Comp::R(y))) => {
                    let m = x.min(y);
                    ap.retain(m);
                    bp.retain(m);
                    op1 = if x > m { Some(Comp::R(x - m)) } else { i1.next() };
                    op2 = if y > m { Some(Comp::R(y - m)) } else { i2.next() };
                }
                (Some(Comp::D(x)), Some(Comp::D(y))) => {
                    // Both deleted the same text: nothing left to do for either.
                    let m = x.min(y);
                    op1 = if x > m { Some(Comp::D(x - m)) } else { i1.next() };
                    op2 = if y > m { Some(Comp::D(y - m)) } else { i2.next() };
                }
                (Some(Comp::D(x)), Some(Comp::R(y))) => {
                    let m = x.min(y);
                    ap.delete(m);
                    op1 = if x > m { Some(Comp::D(x - m)) } else { i1.next() };
                    op2 = if y > m { Some(Comp::R(y - m)) } else { i2.next() };
                }
                (Some(Comp::R(x)), Some(Comp::D(y))) => {
                    let m = x.min(y);
                    bp.delete(m);
                    op1 = if x > m { Some(Comp::R(x - m)) } else { i1.next() };
                    op2 = if y > m { Some(Comp::D(y - m)) } else { i2.next() };
                }
            }
        }
        Ok((ap, bp))
    }

    /// Where a cursor at `index` ends up after this operation (text inserted at
    /// the cursor pushes it right; a deleted range collapses it to the start).
    pub fn transform_index(&self, index: usize) -> usize {
        let mut remaining = index as isize;
        let mut new_index = index as isize;
        for c in &self.ops {
            match c {
                Comp::R(n) => remaining -= *n as isize,
                Comp::I(s) => new_index += char_len(s) as isize,
                Comp::D(n) => {
                    new_index -= remaining.min(*n as isize).max(0);
                    remaining -= *n as isize;
                }
            }
            if remaining < 0 { break; }
        }
        new_index.max(0) as usize
    }
}

// ── Remote cursor position ─────────────────────────────────────────────────────

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RemoteCursorPos {
    pub head: usize,
    pub tail: usize,
}

// ── Peer info ─────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PeerInfo {
    pub site_id:      u64,
    pub name:         String,
    pub color:        u32,
    #[serde(default)]
    pub role:         PeerRole,
    /// Path of the file the peer currently has open (empty = unknown).
    #[serde(default)]
    pub current_file: String,
}

impl Default for PeerRole {
    fn default() -> Self { PeerRole::Viewer }
}

/// A document's full contents at a revision.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct DocSnapshot {
    pub path:    String,
    pub content: String,
    pub rev:     u64,
}

// ── Wire messages ─────────────────────────────────────────────────────────────

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
pub enum CollabMsg {
    // Guest → Host
    Join { peer_name: String },

    // Host → Guest (initial sync). `doc` is the host's current document, if the
    // guest's role may see it; the file list follows separately (`FileList`).
    Welcome {
        your_site_id: u64,
        peers:        Vec<PeerInfo>,
        role:         PeerRole,
        role_perms:   RolePermissions,
        doc:          Option<DocSnapshot>,
    },

    /// Guest → Host: a local edit, based on host revision `base_rev` of `path`.
    Op { path: String, base_rev: u64, op: TextOperation },
    /// Host → Guest: an edit applied by the host as revision `rev`.
    RemoteOp { path: String, rev: u64, site_id: u64, op: TextOperation },
    /// Host → the guest whose `Op` became revision `rev`.
    Ack { path: String, rev: u64 },

    // Cursor broadcast (relayed by the host). `site_id` is filled in by the host.
    CursorUpdate { site_id: u64, cursors: Vec<RemoteCursorPos>, path: String },

    // Host → all
    PeerJoined    { peer: PeerInfo },
    PeerLeft      { site_id: u64 },

    // File access
    FileList      { paths: Vec<String> },          // Host → Guest (re-sent on role/glob change)
    FileRequest   { path: String },                // Guest → Host
    FileResponse  { doc: DocSnapshot },            // Host → Guest (also used to resync)
    FileDenied    { path: String },                // Host → Guest (also: access revoked)
    FileClosed    { path: String },                // Guest → Host: stop sending this doc
    FileClosedByHost { path: String },             // Host → Guest: host closed the document

    // Role management
    RoleConfig    { perms: RolePermissions },            // Host → all (config changed)
    PeerRoleChanged { site_id: u64, role: PeerRole },   // Host → all
    RoleChangeRequest { target_site_id: u64, new_role: PeerRole }, // Moderator → Host

    Error { message: String },
}

// ── Per-document OT state ─────────────────────────────────────────────────────

/// Guest side of one shared document.
#[derive(Default)]
pub struct ClientDoc {
    /// Last host revision incorporated.
    pub rev:      u64,
    /// Sent to the host, not yet acknowledged.
    pub awaiting: Option<TextOperation>,
    /// Local edits made while `awaiting` is outstanding (composed together).
    pub buffer:   Option<TextOperation>,
}

/// Host side of one shared document: the revision counter and recent history.
#[derive(Default)]
pub struct ServerDoc {
    pub rev:       u64,
    history:       VecDeque<TextOperation>,
    history_start: u64, // revision of history[0]'s base
}

const MAX_HISTORY: usize = 5000;

impl ServerDoc {
    fn record(&mut self, op: TextOperation) -> u64 {
        self.history.push_back(op);
        self.rev += 1;
        while self.history.len() > MAX_HISTORY {
            self.history.pop_front();
            self.history_start += 1;
        }
        self.rev
    }

    /// Rebase a guest operation made against `base_rev` onto the current revision.
    /// None if `base_rev` is unknown (too old or in the future) — resync needed.
    pub fn rebase(&self, base_rev: u64, mut op: TextOperation) -> Option<TextOperation> {
        if base_rev < self.history_start || base_rev > self.rev { return None; }
        let skip = (base_rev - self.history_start) as usize;
        for concurrent in self.history.iter().skip(skip) {
            op = TextOperation::transform(&op, concurrent).ok()?.0;
        }
        Some(op)
    }
}

// ── Session state ─────────────────────────────────────────────────────────────

/// Per-guest outgoing queues of `encode_msg` payloads; each guest's writer thread
/// seals them with that connection's `FrameCipher`.
type GuestTxs = Arc<Mutex<HashMap<u64, mpsc::SyncSender<Vec<u8>>>>>;

pub struct CollabSession {
    pub site_id:          u64,
    pub role:             CollabRole,
    pub peers:            Vec<PeerInfo>,
    /// Remote cursor positions, keyed by site_id: (workspace-relative path, cursors).
    pub remote_cursors:   HashMap<u64, (String, Vec<RemoteCursorPos>)>,
    /// Host only: absolute path of the document that was active when hosting started.
    pub doc_path:         String,
    pub session_key:      [u8; 32],
    pub last_cursor_sent: Instant,
    /// Role permission configuration (host-authoritative, broadcast to guests).
    pub role_perms:       RolePermissions,
    /// This peer's own role (None = host).
    pub my_role:          Option<PeerRole>,
    /// Include globs (whitelist; empty = all files visible by role).
    pub include_globs:    Vec<String>,
    /// Exclude globs (blacklist).
    pub exclude_globs:    Vec<String>,
    /// Guest: OT state per open shared document.
    pub client_docs:      HashMap<String, ClientDoc>,
    /// Host: OT state per document any guest has open.
    pub server_docs:      HashMap<String, ServerDoc>,
    /// Host: which documents each guest has open (only they get its edits/cursors).
    pub open_docs:        HashMap<u64, HashSet<String>>,
}

pub enum CollabRole {
    Host {
        guest_txs:  GuestTxs,
        port:       u16,
        invite_str: String,
    },
    Guest {
        /// `encode_msg` payloads; the writer thread seals them.
        to_host: mpsc::Sender<Vec<u8>>,
    },
}

impl CollabSession {
    pub fn is_host(&self) -> bool { matches!(self.role, CollabRole::Host { .. }) }

    pub fn cursor_debounce_elapsed(&self) -> bool {
        self.last_cursor_sent.elapsed() >= Duration::from_millis(50)
    }

    /// Effective permission bits for a peer by site_id (host = site 0 = everything).
    pub fn peer_perms(&self, site_id: u64) -> u16 {
        if site_id == 0 { return perms::ALL; }
        match self.peers.iter().find(|p| p.site_id == site_id) {
            Some(p) => self.role_perms.for_role(p.role),
            None    => 0,
        }
    }

    pub fn peer_role(&self, site_id: u64) -> Option<PeerRole> {
        self.peers.iter().find(|p| p.site_id == site_id).map(|p| p.role)
    }

    /// This session's own permission bits.
    pub fn my_perms(&self) -> u16 {
        match self.my_role {
            None       => perms::ALL,
            Some(role) => self.role_perms.for_role(role),
        }
    }

    /// Host: may `site_id` see `path`? (role, hidden files, include/exclude globs)
    pub fn guest_may_view(&self, site_id: u64, path: &str) -> bool {
        match self.peer_role(site_id) {
            Some(role) => is_safe_rel_path(path)
                && guest_can_view(path, role, &self.role_perms, &self.include_globs, &self.exclude_globs),
            None => false,
        }
    }

    /// Host: may `site_id` edit `path`?
    pub fn guest_may_write(&self, site_id: u64, path: &str) -> bool {
        self.guest_may_view(site_id, path)
            && self.peer_role(site_id).map_or(false, |role| guest_can_write(path, role, &self.role_perms))
    }

    /// Send to one guest (host) or to the host (guest). A guest whose queue is
    /// full (not reading) is disconnected rather than buffered without bound.
    pub fn send_to_site(&self, site_id: u64, msg: &CollabMsg) {
        let Ok(frame) = encode_msg(msg) else { return };
        match &self.role {
            CollabRole::Host { guest_txs, .. } => {
                let mut map = lock_recover(guest_txs);
                if let Some(tx) = map.get(&site_id) {
                    if tx.try_send(frame).is_err() { map.remove(&site_id); }
                }
            }
            CollabRole::Guest { to_host } => { let _ = to_host.send(frame); }
        }
    }

    /// Host: send to every guest (except one) for which `filter` holds.
    pub fn broadcast_where(&self, except: Option<u64>, msg: &CollabMsg, filter: impl Fn(u64) -> bool) {
        let CollabRole::Host { guest_txs, .. } = &self.role else { return };
        let Ok(frame) = encode_msg(msg) else { return };
        let mut map = lock_recover(guest_txs);
        let targets: Vec<u64> = map.keys().copied().filter(|&sid| Some(sid) != except && filter(sid)).collect();
        for sid in targets {
            if let Some(tx) = map.get(&sid) {
                if tx.try_send(frame.clone()).is_err() { map.remove(&sid); }
            }
        }
    }

    /// Host: send to every guest (except one).
    pub fn broadcast_except_msg(&self, except: Option<u64>, msg: &CollabMsg) {
        self.broadcast_where(except, msg, |_| true);
    }

    /// Host: guests that currently have `path` open (and may still see it).
    pub fn guests_with_doc(&self, path: &str) -> Vec<u64> {
        self.open_docs.iter()
            .filter(|(sid, docs)| docs.contains(path) && self.guest_may_view(**sid, path))
            .map(|(&sid, _)| sid)
            .collect()
    }

    /// Invite string (host only; empty string for guests).
    pub fn invite_str(&self) -> &str {
        match &self.role {
            CollabRole::Host { invite_str, .. } => invite_str,
            CollabRole::Guest { .. } => "",
        }
    }

    /// Number of connected peers (not counting self).
    pub fn peer_count(&self) -> usize { self.peers.len() }

    // ── Guest-side OT ────────────────────────────────────────────────────────

    /// Guest: record a local edit to `path` and send it unless one is in flight.
    pub fn client_local_edit(&mut self, path: &str, op: TextOperation) {
        if op.is_noop() { return; }
        let Some(doc) = self.client_docs.get_mut(path) else { return };
        if doc.awaiting.is_none() {
            let msg = CollabMsg::Op { path: path.to_owned(), base_rev: doc.rev, op: op.clone() };
            doc.awaiting = Some(op);
            self.send_to_site(0, &msg);
        } else {
            doc.buffer = Some(match doc.buffer.take() {
                Some(b) => match b.compose(&op) {
                    Ok(c) => c,
                    Err(_) => return,
                },
                None => op,
            });
        }
    }

    /// Guest: the host applied our in-flight operation as `rev`.
    pub fn client_ack(&mut self, path: &str, rev: u64) {
        let Some(doc) = self.client_docs.get_mut(path) else { return };
        doc.rev = rev;
        doc.awaiting = doc.buffer.take();
        if let Some(next) = doc.awaiting.clone() {
            let msg = CollabMsg::Op { path: path.to_owned(), base_rev: rev, op: next };
            self.send_to_site(0, &msg);
        }
    }

    /// Guest: an operation from the host (revision `rev`). Returns it rebased onto
    /// our pending local edits — the version to apply to the local buffer.
    pub fn client_remote_op(&mut self, path: &str, rev: u64, op: TextOperation) -> Option<TextOperation> {
        let doc = self.client_docs.get_mut(path)?;
        let mut op = op;
        if let Some(awaiting) = doc.awaiting.take() {
            let (a2, o2) = TextOperation::transform(&awaiting, &op).ok()?;
            doc.awaiting = Some(a2);
            op = o2;
        }
        if let Some(buffer) = doc.buffer.take() {
            let (b2, o3) = TextOperation::transform(&buffer, &op).ok()?;
            doc.buffer = Some(b2);
            op = o3;
        }
        doc.rev = rev;
        Some(op)
    }

    // ── Host-side OT ─────────────────────────────────────────────────────────

    /// Host: a local edit of the host's own copy of `path`; broadcast it.
    pub fn host_local_edit(&mut self, path: &str, op: TextOperation) {
        if op.is_noop() { return; }
        let Some(doc) = self.server_docs.get_mut(path) else { return }; // nobody shares it
        let rev = doc.record(op.clone());
        let targets = self.guests_with_doc(path);
        let msg = CollabMsg::RemoteOp { path: path.to_owned(), rev, site_id: 0, op };
        self.broadcast_where(None, &msg, |sid| targets.contains(&sid));
    }

    /// Host: accept a guest operation already rebased and applied; record and fan out.
    pub fn host_commit_guest_op(&mut self, from_site_id: u64, path: &str, op: TextOperation) {
        let Some(doc) = self.server_docs.get_mut(path) else { return };
        let rev = doc.record(op.clone());
        let targets = self.guests_with_doc(path);
        let msg = CollabMsg::RemoteOp { path: path.to_owned(), rev, site_id: from_site_id, op };
        self.broadcast_where(Some(from_site_id), &msg, |sid| targets.contains(&sid));
        self.send_to_site(from_site_id, &CollabMsg::Ack { path: path.to_owned(), rev });
    }
}

// ── Host: fan-out helpers ─────────────────────────────────────────────────────

/// Lock a mutex, recovering from poisoning instead of cascading the panic. If one collab
/// thread panics while holding `guest_txs`, the lock is poisoned and every other thread
/// would otherwise panic on its next `.lock().unwrap()`, taking down the whole session.
/// The protected value is just a registry of channel senders, so it stays consistent
/// across a panic — recovering the guard is safe.
fn lock_recover<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ── Encryption / decryption ───────────────────────────────────────────────────

const PAD_OP_SIZE:      usize = 512;
const PAD_CONTROL_SIZE: usize = 64;

fn pad_to(data: &[u8], block: usize) -> Vec<u8> {
    let rem = data.len() % block;
    if rem == 0 { data.to_vec() } else {
        let mut v = data.to_vec();
        v.resize(data.len() + (block - rem), 0);
        v
    }
}

fn unpad(data: &[u8]) -> &[u8] {
    // Payload is always JSON; strip trailing null bytes.
    let len = data.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    &data[..len]
}

/// Serialize and pad a message, ready for a connection's `FrameCipher` to seal.
/// Done once per message (a broadcast shares it); each connection's writer thread
/// then seals it under its own connection id and sequence number.
pub fn encode_msg(msg: &CollabMsg) -> std::io::Result<Vec<u8>> {
    let json = serde_json::to_vec(msg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let pad_size = match msg {
        CollabMsg::Op { .. } | CollabMsg::RemoteOp { .. } => PAD_OP_SIZE,
        _                   => PAD_CONTROL_SIZE,
    };
    Ok(pad_to(&json, pad_size))
}

/// First frame of every connection, host → guest, in the clear: a protocol tag plus a
/// fresh random connection id. Every later frame (starting with the guest's `Join`)
/// is bound to that id, so frames recorded from one connection are useless on another
/// and a captured `Join` can't be replayed to join as that peer.
const HELLO_MAGIC: &[u8; 8] = b"LTCOLAB2";
const CONN_ID_LEN: usize = 32;

/// Directions, part of each frame's associated data so a frame can't be reflected
/// back to its sender.
const DIR_GUEST_TO_HOST: u8 = 0;
const DIR_HOST_TO_GUEST: u8 = 1;

/// One direction of one connection. Each frame is authenticated together with
/// (connection id, direction, sequence number) as associated data; the sequence
/// number isn't sent — both ends count frames, which TCP delivers in order. A
/// replayed, reordered, reflected or dropped frame therefore fails to decrypt, and
/// the connection is closed.
pub struct FrameCipher {
    cipher:  XChaCha20Poly1305,
    conn_id: [u8; CONN_ID_LEN],
    dir:     u8,
    seq:     u64,
}

impl FrameCipher {
    fn new(key: &[u8; 32], conn_id: [u8; CONN_ID_LEN], dir: u8) -> Self {
        Self { cipher: XChaCha20Poly1305::new(key.into()), conn_id, dir, seq: 0 }
    }

    fn aad(&self) -> [u8; 8 + CONN_ID_LEN + 1 + 8] {
        let mut aad = [0u8; 8 + CONN_ID_LEN + 1 + 8];
        aad[..8].copy_from_slice(HELLO_MAGIC);
        aad[8..8 + CONN_ID_LEN].copy_from_slice(&self.conn_id);
        aad[8 + CONN_ID_LEN] = self.dir;
        aad[8 + CONN_ID_LEN + 1..].copy_from_slice(&self.seq.to_be_bytes());
        aad
    }

    /// Encrypt an `encode_msg` payload as the next frame in this direction.
    /// Frame: [4-byte len][24-byte nonce][ciphertext+tag]
    pub fn seal(&mut self, padded: &[u8]) -> std::io::Result<Vec<u8>> {
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);  // 24 bytes
        let aad = self.aad();
        let ct = self.cipher.encrypt(&nonce, Payload { msg: padded, aad: &aad })
            .map_err(|_| std::io::Error::other("encrypt error"))?;
        self.seq += 1;
        let frame_body_len = nonce.len() + ct.len();
        let mut frame = Vec::with_capacity(4 + frame_body_len);
        frame.extend_from_slice(&(frame_body_len as u32).to_be_bytes());
        frame.extend_from_slice(&nonce);
        frame.extend_from_slice(&ct);
        Ok(frame)
    }

    pub fn seal_msg(&mut self, msg: &CollabMsg) -> std::io::Result<Vec<u8>> {
        self.seal(&encode_msg(msg)?)
    }

    /// Decrypt the next frame in this direction (as returned by `read_frame`).
    pub fn open(&mut self, frame: &[u8]) -> Result<CollabMsg, String> {
        if frame.len() < 24 {
            return Err("frame too short".to_owned());
        }
        let (nonce_bytes, ct) = frame.split_at(24);
        let nonce = XNonce::from_slice(nonce_bytes);
        let aad = self.aad();
        let plain = self.cipher.decrypt(nonce, Payload { msg: ct, aad: &aad })
            .map_err(|_| "decryption failed (wrong key, corrupted or replayed frame)".to_owned())?;
        self.seq += 1;
        let unpadded = unpad(&plain);
        if unpadded.is_empty() {
            return Err("empty payload after unpadding".to_owned());
        }
        serde_json::from_slice(unpadded)
            .map_err(|e| format!("json parse error: {e}"))
    }
}

// ── Frame I/O ─────────────────────────────────────────────────────────────────

/// Frame cap once a connection has authenticated (a document can be large).
const MAX_FRAME: usize = 64 * 1024 * 1024;
/// Frame cap before a valid `Join`: an unauthenticated client must not be able to
/// make the host allocate 64 MiB per connection.
const MAX_PREAUTH_FRAME: usize = 64 * 1024;

pub fn read_frame(stream: &mut impl Read, max_len: usize) -> std::io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > max_len {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf)?;
    Ok(buf)
}

pub fn write_frame(stream: &mut impl Write, frame: &[u8]) -> std::io::Result<()> {
    stream.write_all(frame)?;
    stream.flush()
}

// ── File access control ───────────────────────────────────────────────────────

/// A workspace-relative path made only of normal components (no absolute paths,
/// `.` or `..`): the only form guests are allowed to name files in.
///
/// Guest-facing paths always use `/` as the separator. On Windows `\` is a separator
/// too and `:` names an alternate data stream, so either one in a guest path would let
/// it name a file under a different spelling than the permission checks see
/// (`dir\.secret\file` isn't "hidden" when split on `/`): both are rejected there.
pub fn is_safe_rel_path(rel: &str) -> bool {
    if rel.is_empty() { return false; }
    if cfg!(windows) && rel.contains(['\\', ':']) { return false; }
    rel.split('/').all(|c| {
        let mut comps = Path::new(c).components();
        matches!((comps.next(), comps.next()), (Some(Component::Normal(_)), None))
    })
}

/// True if any component of `path` is a dot-file/dir. Splits on both separators so a
/// path that slipped past `is_safe_rel_path` still can't hide a `.secret` component
/// behind a `\`.
fn has_hidden_component(path: &str) -> bool {
    path.split(['/', '\\']).any(|c| c.starts_with('.'))
}

/// Returns true if `path` (relative to workspace root, using `/` separators)
/// is visible to a peer with the given role and permission config.
///
/// Rules applied in order:
/// 1. Role must have VIEW_FILES.
/// 2. If any path component starts with `.`, role must have VIEW_HIDDEN.
/// 3. If `include` is non-empty, path must match at least one include glob.
/// 4. Path must not match any exclude glob.
pub fn guest_can_view(
    path:       &str,
    role:       PeerRole,
    role_perms: &RolePermissions,
    include:    &[String],
    exclude:    &[String],
) -> bool {
    let bits = role_perms.for_role(role);
    if bits & perms::VIEW_FILES == 0 { return false; }

    let is_hidden = has_hidden_component(path);
    if is_hidden && bits & perms::VIEW_HIDDEN == 0 { return false; }

    if !include.is_empty() && !include.iter().any(|g| glob_match(g, path)) {
        return false;
    }
    if exclude.iter().any(|g| glob_match(g, path)) {
        return false;
    }
    true
}

/// Resolve a guest-supplied **relative** path against the workspace `root`, returning the
/// on-disk path only if it stays inside `root` *and* `rel` is that file's one canonical
/// name. Guests are handed workspace-relative paths (see the file-list builder), so
/// anything that isn't a sequence of normal components is rejected: absolute paths
/// (`/etc/passwd` — which `Path::join` would let escape the root entirely) and `.`/`..`
/// traversal. The path is then canonicalized and must come back as exactly `rel`:
///
/// - a symlink inside the workspace can't redirect outside it, nor to a hidden or
///   excluded file inside it (`notes.txt -> .env`), since the permission checks only
///   ever saw the link's name;
/// - on Windows, aliases of a file — other letter case (`SECRET/x` vs an exclude glob of
///   `secret/**`), 8.3 short names (`GIT~1`), trailing dots or spaces — are refused
///   for the same reason.
///
/// Returns None on escape, on any alias, or if the path can't be resolved (e.g. missing
/// file).
pub fn resolve_under_root(root: &Path, rel: &str) -> Option<PathBuf> {
    if !is_safe_rel_path(rel) {
        return None;
    }
    let canon_root = root.canonicalize().ok()?;
    let canon = canon_root.join(rel).canonicalize().ok()?;
    (canonical_rel(&canon_root, &canon)? == rel).then_some(canon)
}

/// `canon` (a canonicalized path) relative to `canon_root`, `/`-separated, or None if it
/// lies outside the root or isn't valid UTF-8.
fn canonical_rel(canon_root: &Path, canon: &Path) -> Option<String> {
    let parts: Option<Vec<&str>> = canon.strip_prefix(canon_root).ok()?
        .components()
        .map(|c| match c {
            Component::Normal(n) => n.to_str(),
            _ => None,
        })
        .collect();
    let parts = parts?;
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// Returns true if `path` is writable for the given role.
pub fn guest_can_write(path: &str, role: PeerRole, role_perms: &RolePermissions) -> bool {
    let bits = role_perms.for_role(role);
    if has_hidden_component(path) {
        bits & perms::WRITE_HIDDEN != 0
    } else {
        bits & perms::WRITE_FILES != 0
    }
}

/// Minimal glob matcher supporting `*` (non-sep), `**` (any), `?` (single non-sep), `{a,b}`.
/// Mirrors the existing `glob_match` in main.rs (duplicated here so collab.rs stays self-contained).
fn glob_match(pattern: &str, path: &str) -> bool {
    fn inner(pat: &[u8], s: &[u8]) -> bool {
        match pat.first() {
            None          => s.is_empty(),
            Some(&b'?')   => !s.is_empty() && s[0] != b'/' && inner(&pat[1..], &s[1..]),
            Some(&b'*') if pat.get(1) == Some(&b'*') => {
                // `**` matches any sequence including `/`
                let rest = &pat[2..];
                let rest = if rest.first() == Some(&b'/') { &rest[1..] } else { rest };
                (0..=s.len()).any(|i| inner(rest, &s[i..]))
            }
            Some(&b'*') => {
                // `*` matches any sequence not containing `/`: stop extending at the
                // first separator (the old check only looked at the last character
                // consumed, so `*.md` also matched `secret/notes.md`).
                let rest = &pat[1..];
                let max = s.iter().position(|&c| c == b'/').unwrap_or(s.len());
                (0..=max).any(|i| inner(rest, &s[i..]))
            }
            Some(&b'{') => {
                // `{a,b,c}` — alternation
                if let Some(close) = pat.iter().position(|&c| c == b'}') {
                    let alts = &pat[1..close];
                    let rest = &pat[close + 1..];
                    let mut start = 0;
                    let mut found = false;
                    for i in 0..=alts.len() {
                        if i == alts.len() || alts[i] == b',' {
                            if inner(&[&alts[start..i], rest].concat(), s) { found = true; }
                            start = i + 1;
                        }
                    }
                    return found;
                }
                !s.is_empty() && pat[0] == s[0] && inner(&pat[1..], &s[1..])
            }
            Some(&c) => !s.is_empty() && c == s[0] && inner(&pat[1..], &s[1..]),
        }
    }
    inner(pattern.as_bytes(), path.as_bytes())
}

// ── Host session startup ──────────────────────────────────────────────────────

/// Per-guest outgoing queue length. A guest that stops reading fills it and is
/// disconnected instead of growing the host's memory without bound.
const GUEST_QUEUE: usize = 4096;
/// Connections that haven't sent a valid Join yet.
const MAX_PENDING_CONNECTIONS: usize = 16;
static PENDING_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);

pub fn start_host(
    port:          u16,
    doc_path:      String,
    proxy:         EventLoopProxy<UserEvent>,
    role_perms:    RolePermissions,
    include_globs: Vec<String>,
    exclude_globs: Vec<String>,
) -> Result<CollabSession, String> {
    // Generate session key
    let mut session_key = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut session_key);

    // Find our LAN IP for the invite string
    let lan_ip = local_ip_str();

    // Encode key as base64url
    use base64::Engine;
    let key_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(session_key);
    let invite_str = format!("lt-collab://{}:{}#{}", lan_ip, port, key_b64);

    let guest_txs: GuestTxs = Arc::new(Mutex::new(HashMap::new()));

    // Bind listener
    let listener = TcpListener::bind(format!("0.0.0.0:{port}"))
        .map_err(|e| format!("Failed to bind port {port}: {e}"))?;

    // Start accept loop
    let guest_txs_accept  = Arc::clone(&guest_txs);
    let proxy_accept      = proxy.clone();
    let session_key_clone = session_key;
    std::thread::spawn(move || {
        let mut next_site_id: u64 = 1;  // host is site 0
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            if PENDING_CONNECTIONS.fetch_add(1, Ordering::AcqRel) >= MAX_PENDING_CONNECTIONS {
                PENDING_CONNECTIONS.fetch_sub(1, Ordering::AcqRel);
                let _ = stream.shutdown(Shutdown::Both);
                continue;
            }
            let _ = stream.set_nodelay(true);
            let sid       = next_site_id;
            next_site_id += 1;
            let color     = PEER_COLORS[sid as usize % PEER_COLORS.len()];

            let proxy2     = proxy_accept.clone();
            let guest_txs2 = Arc::clone(&guest_txs_accept);
            let key2       = session_key_clone;

            std::thread::spawn(move || {
                handle_guest(stream, sid, color, &key2, guest_txs2, proxy2);
            });
        }
    });

    Ok(CollabSession {
        site_id:     0,
        role:        CollabRole::Host { guest_txs, port, invite_str },
        peers:           Vec::new(),
        remote_cursors:  HashMap::new(),
        doc_path,
        session_key,
        last_cursor_sent: Instant::now(),
        role_perms,
        my_role:        None,
        include_globs,
        exclude_globs,
        client_docs:    HashMap::new(),
        server_docs:    HashMap::new(),
        open_docs:      HashMap::new(),
    })
}

/// Per-guest connection handler (runs on its own thread).
/// Waits (briefly, with small frames only) for a valid encrypted Join before the
/// connection is registered for any traffic, then relays messages to the main loop.
fn handle_guest(
    stream:    TcpStream,
    site_id:   u64,
    color:     u32,
    key:       &[u8; 32],
    guest_txs: GuestTxs,
    proxy:     EventLoopProxy<UserEvent>,
) {
    let key = *key;
    let Ok(mut reader) = stream.try_clone() else {
        PENDING_CONNECTIONS.fetch_sub(1, Ordering::AcqRel);
        return;
    };
    let mut writer = stream;
    // ── Hello: a fresh connection id that every later frame is bound to ─────
    let mut conn_id = [0u8; CONN_ID_LEN];
    rand::thread_rng().fill_bytes(&mut conn_id);
    let _ = writer.set_write_timeout(Some(Duration::from_secs(15)));
    let hello = [&HELLO_MAGIC[..], &conn_id[..]].concat();
    let mut hello_frame = (hello.len() as u32).to_be_bytes().to_vec();
    hello_frame.extend_from_slice(&hello);
    if write_frame(&mut writer, &hello_frame).is_err() {
        PENDING_CONNECTIONS.fetch_sub(1, Ordering::AcqRel);
        let _ = writer.shutdown(Shutdown::Both);
        return;
    }
    let mut opener = FrameCipher::new(&key, conn_id, DIR_GUEST_TO_HOST);
    let mut sealer = FrameCipher::new(&key, conn_id, DIR_HOST_TO_GUEST);

    // ── Authenticate: the first frame must decrypt to a Join ────────────────
    let _ = reader.set_read_timeout(Some(Duration::from_secs(15)));
    let join = read_frame(&mut reader, MAX_PREAUTH_FRAME).ok()
        .and_then(|f| opener.open(&f).ok());
    PENDING_CONNECTIONS.fetch_sub(1, Ordering::AcqRel);
    let Some(CollabMsg::Join { peer_name }) = join else {
        let _ = writer.shutdown(Shutdown::Both);
        return;
    };
    let _ = reader.set_read_timeout(None);
    let _ = writer.set_write_timeout(Some(Duration::from_secs(20)));

    // Writer thread for this guest. When it stops (write error or the host
    // dropped our queue as too slow) the socket is shut down so the reader ends too.
    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(GUEST_QUEUE);
    std::thread::spawn(move || {
        for payload in rx {
            let Ok(frame) = sealer.seal(&payload) else { break };
            if write_frame(&mut writer, &frame).is_err() { break; }
        }
        let _ = writer.shutdown(Shutdown::Both);
    });
    lock_recover(&guest_txs).insert(site_id, tx);

    let peer = PeerInfo {
        site_id,
        name: peer_name.chars().take(64).collect(),
        color,
        role: PeerRole::Viewer, // placeholder; host assigns real role
        current_file: String::new(),
    };
    let _ = proxy.send_event(UserEvent::CollabGuestJoined { site_id, peer });

    while let Ok(frame) = read_frame(&mut reader, MAX_FRAME) {
        match opener.open(&frame) {
            Ok(CollabMsg::Join { .. }) => {} // already joined
            Ok(msg) => {
                let _ = proxy.send_event(UserEvent::CollabMessage { from_site_id: site_id, msg });
            }
            Err(e) => {
                // Out of sequence (replayed/injected) or tampered: drop the connection.
                eprintln!("collab: dropping site {site_id}: {e}");
                let _ = reader.shutdown(Shutdown::Both);
                break;
            }
        }
    }
    // Disconnect: remove from guest map, notify main thread
    lock_recover(&guest_txs).remove(&site_id);
    let _ = proxy.send_event(UserEvent::CollabGuestLeft { site_id });
}

// ── Guest session startup ─────────────────────────────────────────────────────

pub fn connect_guest(
    invite_str: &str,
    peer_name:  String,
    proxy:      EventLoopProxy<UserEvent>,
) -> Result<(), String> {
    // Parse: lt-collab://IP:PORT#base64url_key
    let rest = invite_str.strip_prefix("lt-collab://")
        .ok_or("invite string must start with lt-collab://")?;
    let (addr_part, key_part) = rest.split_once('#')
        .ok_or("invite string missing # separator before key")?;

    use base64::Engine;
    let key_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(key_part)
        .map_err(|_| "invalid base64 in invite key")?;
    if key_bytes.len() != 32 {
        return Err(format!("key must be 32 bytes, got {}", key_bytes.len()));
    }
    let mut session_key = [0u8; 32];
    session_key.copy_from_slice(&key_bytes);

    let addr = addr_part.to_owned();

    std::thread::spawn(move || {
        let stream = match TcpStream::connect(&addr) {
            Ok(s) => s,
            Err(e) => {
                let _ = proxy.send_event(UserEvent::CollabError {
                    msg: format!("Failed to connect to {addr}: {e}"),
                });
                return;
            }
        };
        let _ = stream.set_nodelay(true);

        let Ok(mut reader) = stream.try_clone() else {
            let _ = proxy.send_event(UserEvent::CollabError { msg: "socket error".to_owned() });
            return;
        };
        let mut writer = stream;

        // Hello: the host's connection id, which every frame below is bound to.
        let _ = reader.set_read_timeout(Some(Duration::from_secs(15)));
        let hello = read_frame(&mut reader, 64);
        let _ = reader.set_read_timeout(None);
        let conn_id = match hello {
            Ok(h) if h.len() == HELLO_MAGIC.len() + CONN_ID_LEN && h.starts_with(HELLO_MAGIC) => {
                let mut id = [0u8; CONN_ID_LEN];
                id.copy_from_slice(&h[HELLO_MAGIC.len()..]);
                id
            }
            Ok(_) => {
                let _ = proxy.send_event(UserEvent::CollabError {
                    msg: "host speaks a different collab protocol version".to_owned(),
                });
                return;
            }
            Err(e) => {
                let _ = proxy.send_event(UserEvent::CollabError { msg: format!("read error: {e}") });
                return;
            }
        };
        let mut sealer = FrameCipher::new(&session_key, conn_id, DIR_GUEST_TO_HOST);
        let mut opener = FrameCipher::new(&session_key, conn_id, DIR_HOST_TO_GUEST);

        // Send Join
        let join_msg = CollabMsg::Join { peer_name };
        let join_frame = match sealer.seal_msg(&join_msg) {
            Ok(f) => f,
            Err(e) => {
                let _ = proxy.send_event(UserEvent::CollabError { msg: format!("encrypt error: {e}") });
                return;
            }
        };
        if write_frame(&mut writer, &join_frame).is_err() {
            let _ = proxy.send_event(UserEvent::CollabError { msg: "Failed to send Join".to_owned() });
            return;
        }

        // Expect Welcome
        let welcome = loop {
            let frame = match read_frame(&mut reader, MAX_FRAME) {
                Ok(f) => f,
                Err(e) => {
                    let _ = proxy.send_event(UserEvent::CollabError { msg: format!("read error: {e}") });
                    return;
                }
            };
            match opener.open(&frame) {
                Ok(CollabMsg::Welcome { your_site_id, peers, role, role_perms, doc }) => {
                    break (your_site_id, peers, role, role_perms, doc);
                }
                Ok(CollabMsg::Error { message }) => {
                    let _ = proxy.send_event(UserEvent::CollabError { msg: message });
                    return;
                }
                Err(e) => {
                    let _ = proxy.send_event(UserEvent::CollabError {
                        msg: format!("bad key or corrupted Welcome: {e}"),
                    });
                    return;
                }
                _ => { /* ignore other messages before Welcome */ }
            }
        };
        let (site_id, peers, my_role, role_perms, doc) = welcome;

        // Writer thread
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            for payload in rx {
                let Ok(frame) = sealer.seal(&payload) else { break };
                if write_frame(&mut writer, &frame).is_err() { break; }
            }
            let _ = writer.shutdown(Shutdown::Both);
        });

        let session = Box::new(CollabSession {
            site_id,
            role:            CollabRole::Guest { to_host: tx },
            peers,
            remote_cursors:  HashMap::new(),
            doc_path:        String::new(),
            session_key,
            last_cursor_sent: Instant::now(),
            role_perms,
            my_role:         Some(my_role),
            include_globs:   Vec::new(), // host-side only; guests receive a filtered FileList
            exclude_globs:   Vec::new(),
            client_docs:     HashMap::new(),
            server_docs:     HashMap::new(),
            open_docs:       HashMap::new(),
        });

        let _ = proxy.send_event(UserEvent::CollabConnected { session, doc });

        // Reader loop
        loop {
            let frame = match read_frame(&mut reader, MAX_FRAME) {
                Ok(f) => f,
                Err(_) => {
                    let _ = proxy.send_event(UserEvent::CollabDisconnected);
                    break;
                }
            };
            match opener.open(&frame) {
                Ok(msg) => {
                    let _ = proxy.send_event(UserEvent::CollabMessage { from_site_id: 0, msg });
                }
                Err(e) => {
                    // Out of sequence (replayed/injected) or tampered: disconnect.
                    eprintln!("collab: dropping connection to host: {e}");
                    let _ = reader.shutdown(Shutdown::Both);
                    let _ = proxy.send_event(UserEvent::CollabDisconnected);
                    break;
                }
            }
        }
    });

    Ok(())
}

// ── LAN IP detection ──────────────────────────────────────────────────────────

fn local_ip_str() -> String {
    // Connect a UDP socket to a public IP (doesn't actually send anything)
    // to determine which local interface would be used.
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| { s.connect("8.8.8.8:80")?; s.local_addr() })
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_under_root_rejects_traversal() {
        // These are rejected purely on path shape, before touching the filesystem.
        let root = Path::new("/some/workspace");
        assert!(resolve_under_root(root, "/etc/passwd").is_none());      // absolute
        assert!(resolve_under_root(root, "../../etc/passwd").is_none()); // parent escape
        assert!(resolve_under_root(root, "a/../../b").is_none());        // mid-path ..
        assert!(resolve_under_root(root, "./x").is_none());              // explicit `.`
    }

    #[test]
    fn guest_paths_cannot_hide_components_behind_backslashes() {
        let mut role_perms = RolePermissions::default();
        let role = PeerRole::Contributor;
        role_perms.set_for_role(role, perms::VIEW_FILES | perms::WRITE_FILES);
        assert!(guest_can_view("dir/file", role, &role_perms, &[], &[]));
        assert!(!guest_can_view("dir\\.secret\\file", role, &role_perms, &[], &[]));
        assert!(!guest_can_write("dir\\.git\\config", role, &role_perms));
        assert!(is_safe_rel_path("src/main.rs"));
        assert!(!is_safe_rel_path("src//main.rs"));
        assert!(!is_safe_rel_path("src/"));
        if cfg!(windows) {
            assert!(!is_safe_rel_path("dir\\.secret\\file"));
            assert!(!is_safe_rel_path("file.txt:stream"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn resolve_under_root_rejects_symlink_aliases() {
        let dir = std::env::temp_dir().join(format!("lt-collab-alias-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".secret")).unwrap();
        std::fs::write(dir.join(".secret/key"), "k").unwrap();
        std::os::unix::fs::symlink(dir.join(".secret/key"), dir.join("notes.txt")).unwrap();
        assert!(resolve_under_root(&dir, "notes.txt").is_none());
        assert!(resolve_under_root(&dir, ".secret/key").is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn cipher_pair(conn: u8, dir: u8) -> (FrameCipher, FrameCipher) {
        let key = [7u8; 32];
        (FrameCipher::new(&key, [conn; CONN_ID_LEN], dir), FrameCipher::new(&key, [conn; CONN_ID_LEN], dir))
    }

    fn body(frame: &[u8]) -> &[u8] { &frame[4..] }

    #[test]
    fn frames_decrypt_in_order() {
        let (mut tx, mut rx) = cipher_pair(1, DIR_GUEST_TO_HOST);
        for i in 0..3 {
            let f = tx.seal_msg(&CollabMsg::FileRequest { path: format!("f{i}") }).unwrap();
            assert!(matches!(rx.open(body(&f)), Ok(CollabMsg::FileRequest { path }) if path == format!("f{i}")));
        }
    }

    #[test]
    fn replayed_frame_is_rejected() {
        let (mut tx, mut rx) = cipher_pair(1, DIR_GUEST_TO_HOST);
        let f = tx.seal_msg(&CollabMsg::FileRequest { path: "a".into() }).unwrap();
        assert!(rx.open(body(&f)).is_ok());
        assert!(rx.open(body(&f)).is_err());
    }

    #[test]
    fn frame_from_another_connection_or_direction_is_rejected() {
        let key = [7u8; 32];
        let mut tx = FrameCipher::new(&key, [1; CONN_ID_LEN], DIR_GUEST_TO_HOST);
        let join = tx.seal_msg(&CollabMsg::Join { peer_name: "x".into() }).unwrap();
        // A captured Join replayed on a new connection (new id from the host).
        let mut other_conn = FrameCipher::new(&key, [2; CONN_ID_LEN], DIR_GUEST_TO_HOST);
        assert!(other_conn.open(body(&join)).is_err());
        // Reflected back at its sender as if it came the other way.
        let mut reflected = FrameCipher::new(&key, [1; CONN_ID_LEN], DIR_HOST_TO_GUEST);
        assert!(reflected.open(body(&join)).is_err());
    }

    #[test]
    fn resolve_under_root_accepts_contained_file() {
        let dir = std::env::temp_dir().join(format!("lt-collab-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let file = dir.join("sub").join("a.txt");
        std::fs::write(&file, b"hi").unwrap();

        assert_eq!(
            resolve_under_root(&dir, "sub/a.txt"),
            Some(file.canonicalize().unwrap()),
        );
        // A relative path that escapes after joining is still rejected.
        assert!(resolve_under_root(&dir, "sub/../../escape").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn single_star_does_not_cross_directories() {
        assert!(glob_match("*.md", "README.md"));
        assert!(!glob_match("*.md", "secret/notes.md"));
        assert!(glob_match("**/*.md", "secret/notes.md"));
        assert!(glob_match("src/*.rs", "src/main.rs"));
        assert!(!glob_match("src/*.rs", "src/a/b.rs"));
    }

    fn apply(doc: &str, op: &TextOperation) -> String {
        let mut r = Rope::from_str(doc);
        op.apply(&mut r).unwrap();
        r.to_string()
    }

    fn change(doc: &str, start: usize, del: usize, ins: &str) -> TextOperation {
        let len = doc.chars().count();
        TextOperation::from_change(start, del, ins, len - start - del)
    }

    #[test]
    fn disjoint_concurrent_deletes_keep_all_other_text() {
        let doc = "0123456789abcdefghij";
        let a = change(doc, 10, 5, ""); // delete "abcde"
        let b = change(doc, 0, 3, "");  // delete "012"
        let (a2, b2) = TextOperation::transform(&a, &b).unwrap();
        let left = apply(&apply(doc, &a), &b2);
        let right = apply(&apply(doc, &b), &a2);
        assert_eq!(left, "3456789fghij");
        assert_eq!(left, right);
    }

    #[test]
    fn insert_inside_concurrent_delete_converges() {
        let doc = "0123456789";
        let a = change(doc, 2, 5, "");
        let b = change(doc, 4, 0, "XY");
        let (a2, b2) = TextOperation::transform(&a, &b).unwrap();
        assert_eq!(apply(&apply(doc, &a), &b2), apply(&apply(doc, &b), &a2));
    }

    #[test]
    fn compose_matches_sequential_application() {
        let doc = "hello world";
        let a = change(doc, 5, 0, ",");
        let mid = apply(doc, &a);
        let b = change(&mid, 7, 5, "there");
        let c = a.compose(&b).unwrap();
        assert_eq!(apply(doc, &c), apply(&mid, &b));
        assert_eq!(apply(doc, &c), "hello, there");
    }

    #[test]
    fn transform_index_moves_cursors() {
        let doc = "abcdef";
        assert_eq!(change(doc, 0, 0, "XX").transform_index(3), 5);
        assert_eq!(change(doc, 4, 0, "XX").transform_index(3), 3);
        assert_eq!(change(doc, 1, 3, "").transform_index(3), 1);  // inside deletion
        assert_eq!(change(doc, 0, 2, "").transform_index(5), 3);
    }

    /// Simulates the full protocol (host + two guests, random concurrent edits
    /// with arbitrary delivery interleaving) and checks every copy converges.
    #[test]
    fn random_sessions_converge() {
        use rand::{Rng, SeedableRng};
        for seed in 0..300u64 {
            let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
            let initial = "The quick brown fox jumps over the lazy dog.";
            let mut host = Rope::from_str(initial);
            let mut server = ServerDoc::default();
            struct G { text: Rope, doc: ClientDoc, inbox: VecDeque<CollabMsg>, outbox: VecDeque<(u64, TextOperation)> }
            let mut guests: Vec<G> = (0..2).map(|_| G {
                text: Rope::from_str(initial), doc: ClientDoc::default(),
                inbox: VecDeque::new(), outbox: VecDeque::new(),
            }).collect();
            let random_edit = |rng: &mut rand::rngs::StdRng, text: &Rope| {
                let len = text.len_chars();
                let start = rng.gen_range(0..=len);
                let del = rng.gen_range(0..=(len - start).min(4));
                let ins: String = (0..rng.gen_range(0..4)).map(|_| (b'a' + rng.gen_range(0..26)) as char).collect();
                TextOperation::from_change(start, del, &ins, len - start - del)
            };
            for _ in 0..60 {
                match rng.gen_range(0..5) {
                    0 => { // host edits
                        let op = random_edit(&mut rng, &host);
                        op.apply(&mut host).unwrap();
                        let rev = server.record(op.clone());
                        for g in guests.iter_mut() {
                            g.inbox.push_back(CollabMsg::RemoteOp { path: String::new(), rev, site_id: 0, op: op.clone() });
                        }
                    }
                    1 | 2 => { // a guest edits
                        let gi = rng.gen_range(0..guests.len());
                        let g = &mut guests[gi];
                        let op = random_edit(&mut rng, &g.text);
                        op.apply(&mut g.text).unwrap();
                        if g.doc.awaiting.is_none() {
                            g.outbox.push_back((g.doc.rev, op.clone()));
                            g.doc.awaiting = Some(op);
                        } else {
                            g.doc.buffer = Some(match g.doc.buffer.take() { Some(b) => b.compose(&op).unwrap(), None => op });
                        }
                    }
                    3 => { // host processes one guest op
                        let gi = rng.gen_range(0..guests.len());
                        if let Some((base, op)) = guests[gi].outbox.pop_front() {
                            let op = server.rebase(base, op).unwrap();
                            op.apply(&mut host).unwrap();
                            let rev = server.record(op.clone());
                            for (j, g) in guests.iter_mut().enumerate() {
                                if j == gi { g.inbox.push_back(CollabMsg::Ack { path: String::new(), rev }); }
                                else { g.inbox.push_back(CollabMsg::RemoteOp { path: String::new(), rev, site_id: 1, op: op.clone() }); }
                            }
                        }
                    }
                    _ => { // a guest receives one message
                        let gi = rng.gen_range(0..guests.len());
                        let g = &mut guests[gi];
                        match g.inbox.pop_front() {
                            Some(CollabMsg::Ack { rev, .. }) => {
                                g.doc.rev = rev;
                                g.doc.awaiting = g.doc.buffer.take();
                                if let Some(next) = g.doc.awaiting.clone() { g.outbox.push_back((rev, next)); }
                            }
                            Some(CollabMsg::RemoteOp { rev, op, .. }) => {
                                let mut op = op;
                                if let Some(a) = g.doc.awaiting.take() {
                                    let (a2, o2) = TextOperation::transform(&a, &op).unwrap();
                                    g.doc.awaiting = Some(a2); op = o2;
                                }
                                if let Some(b) = g.doc.buffer.take() {
                                    let (b2, o3) = TextOperation::transform(&b, &op).unwrap();
                                    g.doc.buffer = Some(b2); op = o3;
                                }
                                g.doc.rev = rev;
                                op.apply(&mut g.text).unwrap();
                            }
                            _ => {}
                        }
                    }
                }
            }
            // Drain everything.
            loop {
                let mut progressed = false;
                for gi in 0..guests.len() {
                    if let Some((base, op)) = guests[gi].outbox.pop_front() {
                        progressed = true;
                        let op = server.rebase(base, op).unwrap();
                        op.apply(&mut host).unwrap();
                        let rev = server.record(op.clone());
                        for (j, g) in guests.iter_mut().enumerate() {
                            if j == gi { g.inbox.push_back(CollabMsg::Ack { path: String::new(), rev }); }
                            else { g.inbox.push_back(CollabMsg::RemoteOp { path: String::new(), rev, site_id: 1, op: op.clone() }); }
                        }
                    }
                }
                for g in guests.iter_mut() {
                    while let Some(msg) = g.inbox.pop_front() {
                        progressed = true;
                        match msg {
                            CollabMsg::Ack { rev, .. } => {
                                g.doc.rev = rev;
                                g.doc.awaiting = g.doc.buffer.take();
                                if let Some(next) = g.doc.awaiting.clone() { g.outbox.push_back((rev, next)); }
                            }
                            CollabMsg::RemoteOp { rev, op, .. } => {
                                let mut op = op;
                                if let Some(a) = g.doc.awaiting.take() {
                                    let (a2, o2) = TextOperation::transform(&a, &op).unwrap();
                                    g.doc.awaiting = Some(a2); op = o2;
                                }
                                if let Some(b) = g.doc.buffer.take() {
                                    let (b2, o3) = TextOperation::transform(&b, &op).unwrap();
                                    g.doc.buffer = Some(b2); op = o3;
                                }
                                g.doc.rev = rev;
                                op.apply(&mut g.text).unwrap();
                            }
                            _ => {}
                        }
                    }
                }
                if !progressed { break; }
            }
            for g in &guests {
                assert_eq!(g.text.to_string(), host.to_string(), "seed {seed} diverged");
            }
        }
    }
}
