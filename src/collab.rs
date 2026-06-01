// Collaborative editing — end-to-end encrypted real-time co-editing.
//
// Algorithm: Central-server Operational Transformation (Jupiter variant).
// The host is the total-order authority; all ops are sequenced through it.
// No CRDT library is needed because the host provides total ordering.
//
// Encryption: XChaCha20-Poly1305 with a pre-shared session key (PSK).
// The host generates a random 32-byte key and embeds it in the invite string:
//   lt-collab://IP:PORT#base64url(key)
// Each message is padded to a multiple of 512 bytes, then encrypted with a
// fresh random 24-byte nonce.  The Poly1305 tag provides authentication.
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

use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng};
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

// ── Terminal info ─────────────────────────────────────────────────────────────

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TermInfo {
    pub term_id: usize,
    pub title:   String,
    pub shared:  bool,
}

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

// ── Text operation primitives ──────────────────────────────────────────────────

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum TextOp {
    Insert { pos: usize, text: String, site_id: u64 },
    Delete { pos: usize, len: usize },
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

// ── Wire messages ─────────────────────────────────────────────────────────────

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
pub enum CollabMsg {
    // Guest → Host
    Join { peer_name: String },

    // Host → Guest (initial sync — includes role config and filtered file list)
    Welcome {
        your_site_id: u64,
        doc_text:     String,
        server_clock: u64,
        peers:        Vec<PeerInfo>,
        role:         PeerRole,
        role_perms:   RolePermissions,
        file_list:    Vec<String>,
    },

    // Client → Host → all others (host also applies to itself)
    Op {
        site_id: u64,
        clock:   u64,
        ops:     Vec<TextOp>,
        path:    String,
    },

    // Host → sending client
    Ack { clock: u64, path: String },

    // Cursor broadcast with file context (either direction via host)
    CursorUpdate { site_id: u64, cursors: Vec<RemoteCursorPos>, path: String },

    // Host → all
    PeerJoined    { peer: PeerInfo },
    PeerLeft      { site_id: u64 },

    // File access
    FileList      { paths: Vec<String> },                // Host → Guest (re-sent on role/glob change)
    FileRequest   { path: String },                      // Guest → Host
    FileResponse  { path: String, content: String, server_clock: u64 }, // Host → Guest
    FileDenied    { path: String },                      // Host → Guest
    FileSaved     { path: String, content: String },     // Host → all VIEW_FILES peers (on save)

    // Role management
    RoleConfig    { perms: RolePermissions },            // Host → all (config changed)
    PeerRoleChanged { site_id: u64, role: PeerRole },   // Host → all
    RoleChangeRequest { target_site_id: u64, new_role: PeerRole }, // Moderator → Host

    // Terminal sharing
    TerminalList  { terms: Vec<TermInfo> },              // Host → VIEW_TERMINALS peers
    TerminalOutput{ term_id: usize, data: Vec<u8> },     // Host → VIEW_TERMINALS peers
    TerminalInput { term_id: usize, data: Vec<u8> },     // Guest → Host (OPEN_TERMINALS required)
    TerminalOpen  { },                                   // Guest → Host (request new shell)
    TerminalOpened{ term_id: usize },                    // Host → requesting guest

    Error { message: String },
}

// ── In-flight ops (guest side) ────────────────────────────────────────────────

#[derive(Clone)]
pub struct InflightOp {
    pub clock: u64,
    pub ops:   Vec<TextOp>,
}

// ── Session state ─────────────────────────────────────────────────────────────

pub struct CollabSession {
    pub site_id:          u64,
    pub role:             CollabRole,
    pub peers:            Vec<PeerInfo>,
    /// In-flight ops per file path (guest only; host acks immediately).
    pub inflight:         HashMap<String, Vec<InflightOp>>,
    pub local_clock:      u64,
    pub server_clock:     u64,
    /// Remote cursor positions, keyed by site_id.  Value includes path.
    pub remote_cursors:   HashMap<u64, (String, Vec<RemoteCursorPos>)>,
    pub send_tx:          mpsc::Sender<Vec<u8>>,
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
}

pub enum CollabRole {
    Host {
        guest_txs:    Arc<Mutex<HashMap<u64, mpsc::Sender<Vec<u8>>>>>,
        next_site_id: u64,
        port:         u16,
        invite_str:   String,
    },
    Guest,
}

impl CollabSession {
    /// Enqueue a local op for `path`: add to per-path inflight and send to server.
    pub fn send_local_op(&mut self, path: String, ops: Vec<TextOp>) {
        self.local_clock += 1;
        let clock = self.local_clock;
        let msg = CollabMsg::Op {
            site_id: self.site_id,
            clock,
            ops:     ops.clone(),
            path:    path.clone(),
        };
        self.inflight.entry(path).or_default().push(InflightOp { clock, ops });
        if let Ok(frame) = encrypt_msg(&self.session_key, &msg) {
            let _ = self.send_tx.send(frame);
        }
    }

    /// Send a cursor update with file context (debounced by caller).
    pub fn send_cursor_update(&mut self, path: String, cursors: Vec<RemoteCursorPos>) {
        let msg = CollabMsg::CursorUpdate { site_id: self.site_id, cursors, path };
        if let Ok(frame) = encrypt_msg(&self.session_key, &msg) {
            let _ = self.send_tx.send(frame);
        }
        self.last_cursor_sent = Instant::now();
    }

    pub fn cursor_debounce_elapsed(&self) -> bool {
        self.last_cursor_sent.elapsed() >= Duration::from_millis(50)
    }

    /// Return the effective permission bits for a peer by site_id.
    /// Returns `perms::ALL` for the host (site 0).
    pub fn peer_perms(&self, site_id: u64) -> u16 {
        if site_id == 0 { return perms::ALL; }
        match self.peers.iter().find(|p| p.site_id == site_id) {
            Some(p) => self.role_perms.for_role(p.role),
            None    => 0,
        }
    }

    /// Return this session's own permission bits.
    pub fn my_perms(&self) -> u16 {
        match self.my_role {
            None       => perms::ALL,
            Some(role) => self.role_perms.for_role(role),
        }
    }

    /// Send a message directly to a specific guest (host only) or to the host (guest).
    pub fn send_to_site(&self, site_id: u64, msg: &CollabMsg) {
        if let Ok(frame) = encrypt_msg(&self.session_key, msg) {
            match &self.role {
                CollabRole::Host { guest_txs, .. } => {
                    if let Some(tx) = lock_recover(guest_txs).get(&site_id) {
                        let _ = tx.send(frame);
                    }
                }
                CollabRole::Guest => {
                    let _ = self.send_tx.send(frame);
                }
            }
        }
    }

    /// Broadcast a message to all guests except one (host only).
    /// As a guest, sends to the host (ignores except_site_id).
    pub fn broadcast_except_msg(&self, except_site_id: Option<u64>, msg: &CollabMsg) {
        if let Ok(frame) = encrypt_msg(&self.session_key, msg) {
            match &self.role {
                CollabRole::Host { guest_txs, .. } => {
                    broadcast_to_guests(guest_txs, except_site_id, frame);
                }
                CollabRole::Guest => {
                    let _ = self.send_tx.send(frame);
                }
            }
        }
    }

    /// Invite string (host only; empty string for guests).
    pub fn invite_str(&self) -> &str {
        match &self.role {
            CollabRole::Host { invite_str, .. } => invite_str,
            CollabRole::Guest => "",
        }
    }

    /// Number of connected peers (not counting self).
    pub fn peer_count(&self) -> usize { self.peers.len() }
}

// ── Host: fan-out broadcast ───────────────────────────────────────────────────

/// Lock a mutex, recovering from poisoning instead of cascading the panic. If one collab
/// thread panics while holding `guest_txs`, the lock is poisoned and every other thread
/// would otherwise panic on its next `.lock().unwrap()`, taking down the whole session.
/// The protected value is just a registry of channel senders, so it stays consistent
/// across a panic — recovering the guard is safe.
fn lock_recover<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn broadcast_to_guests(
    guest_txs: &Arc<Mutex<HashMap<u64, mpsc::Sender<Vec<u8>>>>>,
    except_site_id: Option<u64>,
    frame: Vec<u8>,
) {
    let map = lock_recover(guest_txs);
    for (&sid, tx) in map.iter() {
        if except_site_id == Some(sid) { continue; }
        let _ = tx.send(frame.clone());
    }
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

pub fn encrypt_msg(key: &[u8; 32], msg: &CollabMsg) -> std::io::Result<Vec<u8>> {
    let json = serde_json::to_vec(msg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let pad_size = match msg {
        CollabMsg::Op { .. } => PAD_OP_SIZE,
        _                   => PAD_CONTROL_SIZE,
    };
    let padded = pad_to(&json, pad_size);

    let cipher = XChaCha20Poly1305::new(key.into());
    let nonce  = XChaCha20Poly1305::generate_nonce(&mut OsRng);  // 24 bytes
    let ct     = cipher.encrypt(&nonce, padded.as_ref())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "encrypt error"))?;

    // Frame: [4-byte len][24-byte nonce][ciphertext+tag]
    let frame_body_len = nonce.len() + ct.len();
    let mut frame = Vec::with_capacity(4 + frame_body_len);
    frame.extend_from_slice(&(frame_body_len as u32).to_be_bytes());
    frame.extend_from_slice(&nonce);
    frame.extend_from_slice(&ct);
    Ok(frame)
}

pub fn decrypt_frame(key: &[u8; 32], frame: &[u8]) -> Result<CollabMsg, String> {
    if frame.len() < 24 {
        return Err("frame too short".to_owned());
    }
    let (nonce_bytes, ct) = frame.split_at(24);
    let nonce  = XNonce::from_slice(nonce_bytes);
    let cipher = XChaCha20Poly1305::new(key.into());
    let plain  = cipher.decrypt(nonce, ct)
        .map_err(|_| "decryption failed (wrong key or corrupted frame)".to_owned())?;
    let unpadded = unpad(&plain);
    if unpadded.is_empty() {
        return Err("empty payload after unpadding".to_owned());
    }
    serde_json::from_slice(unpadded)
        .map_err(|e| format!("json parse error: {e}"))
}

// ── Frame I/O ─────────────────────────────────────────────────────────────────

pub fn read_frame(stream: &mut impl Read) -> std::io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > 64 * 1024 * 1024 { // 64 MiB sanity cap
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

// ── Operational Transformation ────────────────────────────────────────────────

/// Transform Insert A against Insert B. Returns new position for A.
fn xform_ii(a_pos: usize, _a_len: usize, a_site: u64, b_pos: usize, b_len: usize, b_site: u64) -> usize {
    if b_pos < a_pos { a_pos + b_len }
    else if b_pos > a_pos { a_pos }
    else if b_site < a_site { a_pos + b_len }
    else { a_pos }
}

/// Transform Insert against Delete. Returns new position for the Insert.
fn xform_id(ins_pos: usize, del_pos: usize, del_len: usize) -> usize {
    if ins_pos <= del_pos { ins_pos }
    else if ins_pos >= del_pos + del_len { ins_pos - del_len }
    else { del_pos }
}

/// Transform Delete against Insert. Returns new (pos, len) for the Delete.
fn xform_di(del_pos: usize, del_len: usize, ins_pos: usize, ins_len: usize) -> (usize, usize) {
    if ins_pos <= del_pos { (del_pos + ins_len, del_len) }
    else if ins_pos >= del_pos + del_len { (del_pos, del_len) }
    else { (del_pos, del_len + ins_len) }
}

/// Transform Delete against Delete. Returns None if A is fully consumed by B.
fn xform_dd(a_pos: usize, a_len: usize, b_pos: usize, b_len: usize) -> Option<(usize, usize)> {
    let a_left  = b_pos.saturating_sub(a_pos);
    let a_right = (a_pos + a_len).saturating_sub(b_pos + b_len);
    if a_left + a_right == 0 { return None; }
    let new_pos = a_pos.min(b_pos);
    Some((new_pos, a_left + a_right))
}

/// Transform a single op A against a single op B (B has already been applied).
fn xform_op(a: TextOp, b: &TextOp) -> Option<TextOp> {
    match (a, b) {
        (TextOp::Insert { pos: ap, text: at, site_id: asid },
         TextOp::Insert { pos: bp, text: bt, site_id: bsid }) => {
            let new_pos = xform_ii(ap, at.chars().count(), asid, *bp, bt.chars().count(), *bsid);
            Some(TextOp::Insert { pos: new_pos, text: at, site_id: asid })
        }
        (TextOp::Insert { pos: ap, text: at, site_id: asid },
         TextOp::Delete { pos: dp, len: dl }) => {
            let new_pos = xform_id(ap, *dp, *dl);
            Some(TextOp::Insert { pos: new_pos, text: at, site_id: asid })
        }
        (TextOp::Delete { pos: dp, len: dl },
         TextOp::Insert { pos: ip, text: it, site_id: _ }) => {
            let (np, nl) = xform_di(dp, dl, *ip, it.chars().count());
            Some(TextOp::Delete { pos: np, len: nl })
        }
        (TextOp::Delete { pos: ap, len: al },
         TextOp::Delete { pos: bp, len: bl }) => {
            xform_dd(ap, al, *bp, *bl).map(|(p, l)| TextOp::Delete { pos: p, len: l })
        }
    }
}

/// Transform a compound op A (Vec<TextOp>) against a single op B.
/// Updates A in place; returns the mirrored transformation of B against A.
fn xform_compound_against_op(a_ops: &mut Vec<TextOp>, b: TextOp) -> Option<TextOp> {
    let mut b_current = Some(b);
    for a_op in a_ops.iter_mut() {
        if let Some(b_op) = b_current.take() {
            let new_a = xform_op(a_op.clone(), &b_op);
            let new_b = xform_op(b_op, a_op);
            if let Some(na) = new_a { *a_op = na; }
            b_current = new_b;
        }
    }
    b_current
}

/// Transform remote_ops against inflight_ops in place.
/// On return: remote_ops has been rebased on top of inflight_ops,
/// and inflight_ops has been rebased on top of (the original) remote_ops.
pub fn integrate_remote_against_inflight(remote_ops: &mut Vec<TextOp>, inflight: &mut Vec<InflightOp>) {
    for inflight_op in inflight.iter_mut() {
        let mut new_remote = Vec::with_capacity(remote_ops.len());
        for r_op in remote_ops.drain(..) {
            let r_transformed = xform_compound_against_op(&mut inflight_op.ops, r_op);
            if let Some(t) = r_transformed { new_remote.push(t); }
        }
        *remote_ops = new_remote;
    }
}

// ── Cursor adjustment ─────────────────────────────────────────────────────────
//
// Rust equivalent of `adjustOffset()` from multi-user-editor/Editor.tsx.
// In local-text cursor positions are usize char offsets into the Rope —
// much simpler than the DOM positions that required heavy iteration in the
// browser-based project.

pub fn adjust_cursor_pos(cursor_pos: usize, op: &TextOp, local_site_id: u64) -> usize {
    match op {
        TextOp::Insert { pos, text, site_id: remote_sid } => {
            let len = text.chars().count();
            if *pos < cursor_pos {
                cursor_pos + len    // insert before cursor → shift right
            } else if *pos == cursor_pos && *remote_sid < local_site_id {
                cursor_pos + len    // same pos, remote wins tiebreak → cursor shifts right
            } else {
                cursor_pos          // insert at or after cursor → no change
            }
        }
        TextOp::Delete { pos, len } => {
            let end = pos + len;
            if end <= cursor_pos     { cursor_pos - len }   // delete entirely before
            else if *pos < cursor_pos { *pos }              // cursor inside deleted region → clamp
            else                     { cursor_pos }         // delete at or after cursor
        }
    }
}

/// Apply a list of ops to adjust a cursor (head, tail) pair.
pub fn adjust_cursor(head: usize, tail: usize, ops: &[TextOp], local_site_id: u64) -> (usize, usize) {
    let mut h = head;
    let mut t = tail;
    for op in ops {
        h = adjust_cursor_pos(h, op, local_site_id);
        t = adjust_cursor_pos(t, op, local_site_id);
    }
    (h, t)
}

// ── Apply ops to a Rope (no undo entry) ──────────────────────────────────────

pub fn apply_ops_to_rope(rope: &mut Rope, ops: &[TextOp]) {
    for op in ops {
        match op {
            TextOp::Insert { pos, text, .. } => {
                let p = (*pos).min(rope.len_chars());
                rope.insert(p, text);
            }
            TextOp::Delete { pos, len } => {
                let p   = (*pos).min(rope.len_chars());
                let end = (p + len).min(rope.len_chars());
                if p < end { rope.remove(p..end); }
            }
        }
    }
}

// ── Op extraction from Rope diff ─────────────────────────────────────────────
//
// Finds the common prefix and suffix char counts to extract what changed.
// Produces at most one Delete + one Insert.  This is correct for all editor
// operations because the existing code processes multi-cursor edits left-to-right
// with delta tracking, producing one contiguous changed region.

pub fn extract_ops(before: &Rope, after: &Rope, site_id: u64) -> Vec<TextOp> {
    // Collect to char vecs for simple character comparison.
    // Edits are small and this runs only when collab is active.
    let before_chars: Vec<char> = before.chars().collect();
    let after_chars:  Vec<char> = after.chars().collect();

    let prefix = before_chars.iter().zip(after_chars.iter())
        .take_while(|(a, b)| a == b).count();

    let max_suffix = before_chars.len().saturating_sub(prefix)
        .min(after_chars.len().saturating_sub(prefix));
    let suffix = before_chars[prefix..].iter().rev()
        .zip(after_chars[prefix..].iter().rev())
        .take(max_suffix)
        .take_while(|(a, b)| a == b)
        .count();

    let deleted = before_chars.len() - prefix - suffix;
    let inserted: String = after_chars[prefix..after_chars.len() - suffix].iter().collect();

    let mut ops = Vec::new();
    if deleted > 0 {
        ops.push(TextOp::Delete { pos: prefix, len: deleted });
    }
    if !inserted.is_empty() {
        // After applying the delete (if any), the insertion position is `prefix`.
        ops.push(TextOp::Insert { pos: prefix, text: inserted, site_id });
    }
    ops
}

// ── File access control ───────────────────────────────────────────────────────

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

    let is_hidden = path.split('/').any(|c| c.starts_with('.'));
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
/// on-disk path only if it stays inside `root`. Guests are handed workspace-relative paths
/// (see the file-list builder), so anything that isn't a sequence of normal components is
/// rejected: absolute paths (`/etc/passwd` — which `Path::join` would let escape the root
/// entirely) and `.`/`..` traversal. The path is then canonicalized and re-checked for
/// containment so a symlink inside the workspace can't redirect outside it either. Returns
/// None on escape or if the path can't be resolved (e.g. missing file).
pub fn resolve_under_root(root: &Path, rel: &str) -> Option<PathBuf> {
    if Path::new(rel).components().any(|c| !matches!(c, Component::Normal(_))) {
        return None;
    }
    let canon_root = root.canonicalize().ok()?;
    let canon = canon_root.join(rel).canonicalize().ok()?;
    canon.starts_with(&canon_root).then_some(canon)
}

/// Returns true if `path` is writable for the given role.
pub fn guest_can_write(path: &str, role: PeerRole, role_perms: &RolePermissions) -> bool {
    let bits = role_perms.for_role(role);
    let is_hidden = path.split('/').any(|c| c.starts_with('.'));
    if is_hidden {
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
                // `*` matches any sequence not containing `/`
                let rest = &pat[1..];
                (0..=s.len()).filter(|&i| i == 0 || s[i-1] != b'/').any(|i| inner(rest, &s[i..]))
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

    let guest_txs: Arc<Mutex<HashMap<u64, mpsc::Sender<Vec<u8>>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Bind listener
    let listener = TcpListener::bind(format!("0.0.0.0:{port}"))
        .map_err(|e| format!("Failed to bind port {port}: {e}"))?;

    // Channel for the host to broadcast to all guests.
    let (host_tx, host_rx) = mpsc::channel::<Vec<u8>>();

    // Start accept loop
    let guest_txs_accept  = Arc::clone(&guest_txs);
    let proxy_accept      = proxy.clone();
    let session_key_clone = session_key;
    std::thread::spawn(move || {
        let mut next_site_id: u64 = 1;  // host is site 0
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let _ = stream.set_nodelay(true);
            let sid       = next_site_id;
            next_site_id += 1;
            let color     = PEER_COLORS[sid as usize % PEER_COLORS.len()];

            let (guest_tx, guest_rx) = mpsc::channel::<Vec<u8>>();
            lock_recover(&guest_txs_accept).insert(sid, guest_tx);

            let proxy2     = proxy_accept.clone();
            let guest_txs2 = Arc::clone(&guest_txs_accept);
            let key2       = session_key_clone;

            std::thread::spawn(move || {
                handle_guest(stream, sid, color, &key2, guest_rx, guest_txs2, proxy2);
            });
        }
    });

    // Host's own writer thread (broadcasts to all guests)
    let guest_txs_writer = Arc::clone(&guest_txs);
    std::thread::spawn(move || {
        for frame in host_rx {
            broadcast_to_guests(&guest_txs_writer, None, frame);
        }
    });

    Ok(CollabSession {
        site_id:     0,
        role:        CollabRole::Host {
            guest_txs,
            next_site_id: 1,
            port,
            invite_str,
        },
        peers:           Vec::new(),
        inflight:        HashMap::new(),
        local_clock:     0,
        server_clock:    0,
        remote_cursors:  HashMap::new(),
        send_tx:         host_tx,
        doc_path,
        session_key,
        last_cursor_sent: Instant::now(),
        role_perms,
        my_role:        None,
        include_globs,
        exclude_globs,
    })
}

/// Per-guest connection handler (runs on its own thread).
/// Waits for Join, notifies main thread, then relays subsequent messages.
fn handle_guest(
    stream:    TcpStream,
    site_id:   u64,
    color:     u32,
    key:       &[u8; 32],
    rx:        mpsc::Receiver<Vec<u8>>,
    guest_txs: Arc<Mutex<HashMap<u64, mpsc::Sender<Vec<u8>>>>>,
    proxy:     EventLoopProxy<UserEvent>,
) {
    let mut reader = stream.try_clone().expect("clone stream");
    let mut writer = stream;

    // Writer thread for this guest
    std::thread::spawn(move || {
        for frame in rx {
            if write_frame(&mut writer, &frame).is_err() { break; }
        }
    });

    // Read-side: wait for Join, then loop on messages
    let key = *key;  // copy for this thread
    loop {
        let frame = match read_frame(&mut reader) {
            Ok(f) => f,
            Err(_) => {
                // Disconnect: remove from guest map, notify main thread
                lock_recover(&guest_txs).remove(&site_id);
                let _ = proxy.send_event(UserEvent::CollabGuestLeft { site_id });
                return;
            }
        };
        let msg = match decrypt_frame(&key, &frame) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("collab: decrypt error from site {site_id}: {e}");
                continue;
            }
        };
        match msg {
            CollabMsg::Join { peer_name } => {
                // Notify main thread with peer info; it will assign a role,
                // send Welcome, and broadcast PeerJoined.
                let peer = PeerInfo {
                    site_id,
                    name: peer_name,
                    color,
                    role: PeerRole::Viewer, // placeholder; host assigns real role
                    current_file: String::new(),
                };
                let _ = proxy.send_event(UserEvent::CollabGuestJoined { site_id, peer });
            }
            other => {
                // Forward all other messages to main thread
                let _ = proxy.send_event(UserEvent::CollabMessage { from_site_id: site_id, msg: other });
            }
        }
    }
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

        let mut reader = stream.try_clone().expect("clone");
        let mut writer = stream;

        // Send Join
        let join_msg = CollabMsg::Join { peer_name };
        let join_frame = match encrypt_msg(&session_key, &join_msg) {
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
            let frame = match read_frame(&mut reader) {
                Ok(f) => f,
                Err(e) => {
                    let _ = proxy.send_event(UserEvent::CollabError { msg: format!("read error: {e}") });
                    return;
                }
            };
            match decrypt_frame(&session_key, &frame) {
                Ok(CollabMsg::Welcome { your_site_id, doc_text, server_clock, peers, role, role_perms, file_list }) => {
                    break (your_site_id, doc_text, server_clock, peers, role, role_perms, file_list);
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
        let (site_id, doc_text, server_clock, peers, my_role, role_perms, file_list) = welcome;

        // Writer thread
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            for frame in rx {
                if write_frame(&mut writer, &frame).is_err() { break; }
            }
        });

        let session = Box::new(CollabSession {
            site_id,
            role:            CollabRole::Guest,
            peers:           peers.clone(),
            inflight:        HashMap::new(),
            local_clock:     0,
            server_clock,
            remote_cursors:  HashMap::new(),
            send_tx:         tx,
            doc_path:        String::new(), // set from tab path in main after CollabConnected
            session_key,
            last_cursor_sent: Instant::now(),
            role_perms,
            my_role:         Some(my_role),
            include_globs:   Vec::new(), // host-side only; guests receive filtered FileList
            exclude_globs:   Vec::new(),
        });

        let _ = proxy.send_event(UserEvent::CollabConnected { session, doc_text, peers, file_list });

        // Reader loop
        let key = session_key;
        loop {
            let frame = match read_frame(&mut reader) {
                Ok(f) => f,
                Err(_) => {
                    let _ = proxy.send_event(UserEvent::CollabDisconnected);
                    break;
                }
            };
            match decrypt_frame(&key, &frame) {
                Ok(msg) => {
                    let _ = proxy.send_event(UserEvent::CollabMessage { from_site_id: 0, msg });
                }
                Err(e) => { eprintln!("collab: decrypt error: {e}"); }
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
}
