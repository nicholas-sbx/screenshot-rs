//! The clipboard on X11. Flatshot owns the CLIPBOARD selection (with a
//! hidden window of its own) and answers each program that asks for it;
//! large data goes in pieces, the INCR way. Pasting asks the owner for
//! text and reads its answer. A one-off capture hands what it copied to
//! the desktop's clipboard manager (SAVE_TARGETS), when there is one, so
//! it can end.

use std::collections::HashMap;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    self, AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, EventMask, PropMode, PropertyNotifyEvent, SelectionClearEvent, SelectionNotifyEvent,
    SelectionRequestEvent,
};
use x11rb::wrapper::ConnectionExt as _;

use super::X11;
use crate::clipboard::{MAX_PASTE, STALL, Slot, WAIT_FOR_DATA};
use crate::ui::session::Waker;

/// The most sent in one piece (well within any server's request size).
const CHUNK: usize = 128 * 1024;
/// How long a clipboard manager may take to save what we offer.
const SAVE_WAIT: Duration = Duration::from_secs(3);
/// The text types a paste asks for, best first.
const PASTE_TYPES: [&str; 3] = ["UTF8_STRING", "text/plain;charset=utf-8", "STRING"];

#[derive(Default)]
pub struct Clipboard {
    offered: Option<Offered>,
    /// requests for data still being made, and until when they wait
    waiting: Vec<(SelectionRequestEvent, Instant)>,
    /// big answers going out in pieces
    sending: Vec<Incr>,
    paste: Option<Paste>,
    /// a clipboard manager saving what's offered: until when it may take
    saving: Option<Instant>,
    save_asked: bool,
    /// saved by the clipboard manager: nothing left to hold on for
    saved: bool,
    atoms: HashMap<String, xproto::Atom>,
}

struct Offered {
    /// the types, and their atoms
    mimes: Vec<(xproto::Atom, String)>,
    slot: Arc<Slot>,
    /// other data for some of the types
    extra: Vec<(String, Arc<Vec<u8>>)>,
    since: xproto::Timestamp,
}

struct Incr {
    requestor: xproto::Window,
    property: xproto::Atom,
    kind: xproto::Atom,
    data: Arc<Vec<u8>>,
    at: usize,
    until: Instant,
}

struct Paste {
    sink: PasteSink,
    wake: Waker,
    /// the types still to try
    types: Vec<xproto::Atom>,
    /// the type asked for now
    asked: xproto::Atom,
    kind: xproto::Atom,
    got: Vec<u8>,
    /// the most taken
    max: usize,
    /// coming in pieces
    incr: bool,
    until: Instant,
}

/// Where a paste's data goes: as text, or as it is with its type.
enum PasteSink {
    Text(mpsc::Sender<Option<String>>),
    Bytes(mpsc::Sender<Option<(String, Vec<u8>)>>),
}

enum Converted {
    Done,
    Refused,
    /// the data is still being made
    Later,
}

impl X11 {
    fn atom(&mut self, name: &str) -> Option<xproto::Atom> {
        if let Some(a) = self.clip.atoms.get(name) {
            return Some(*a);
        }
        let a = self.conn.intern_atom(false, name.as_bytes()).ok()?.reply().ok()?.atom;
        self.clip.atoms.insert(name.to_string(), a);
        Some(a)
    }

    fn atom_name(&mut self, atom: xproto::Atom) -> Option<String> {
        if let Some((name, _)) = self.clip.atoms.iter().find(|(_, a)| **a == atom) {
            return Some(name.clone());
        }
        let name = String::from_utf8(self.conn.get_atom_name(atom).ok()?.reply().ok()?.name).ok()?;
        self.clip.atoms.insert(name.clone(), atom);
        Some(name)
    }

    pub fn offer(&mut self, mimes: Vec<String>, slot: Arc<Slot>) -> bool {
        self.offer_parts(mimes, slot, Vec::new())
    }

    pub fn offer_each(&mut self, mut parts: Vec<(String, Arc<Vec<u8>>)>) -> bool {
        if parts.is_empty() {
            return false;
        }
        let mimes = parts.iter().map(|p| p.0.clone()).collect();
        let first = parts.remove(0);
        self.offer_parts(mimes, Slot::ready(first.1), parts)
    }

    fn offer_parts(&mut self, mimes: Vec<String>, slot: Arc<Slot>, extra: Vec<(String, Arc<Vec<u8>>)>) -> bool {
        let atoms: Vec<(xproto::Atom, String)> = mimes.into_iter().filter_map(|m| Some((self.atom(&m)?, m))).collect();
        if atoms.is_empty() {
            return false;
        }
        let clipboard = self.atoms.CLIPBOARD;
        if self.conn.set_selection_owner(self.helper, clipboard, self.time).is_err() {
            return false;
        }
        let owner = self.conn.get_selection_owner(clipboard).ok().and_then(|c| c.reply().ok()).map(|r| r.owner);
        if owner != Some(self.helper) {
            return false;
        }
        self.refuse_waiting();
        self.clip.offered = Some(Offered { mimes: atoms, slot, extra, since: self.time });
        self.clip.saving = None;
        self.clip.save_asked = false;
        self.clip.saved = false;
        true
    }

    pub fn holds(&self) -> bool {
        self.clip.offered.is_some() && !self.clip.saved
    }

    pub fn withdraw(&mut self) {
        if let Some(o) = self.clip.offered.take() {
            o.slot.abandon();
            let _ = self.conn.set_selection_owner(x11rb::NONE, self.atoms.CLIPBOARD, self.time);
        }
        self.refuse_waiting();
    }

    /// Does the desktop have a clipboard manager (to keep what's copied
    /// once Flatshot is gone)?
    pub fn has_clipboard_manager(&self) -> bool {
        let manager = self.conn.get_selection_owner(self.atoms.CLIPBOARD_MANAGER).ok().and_then(|c| c.reply().ok()).map(|r| r.owner);
        manager.is_some_and(|m| m != x11rb::NONE)
    }

    /// Let the desktop's clipboard manager keep what's offered, so a
    /// one-off capture can end. False if there's none.
    pub fn hand_off_clipboard(&mut self) -> bool {
        if self.clip.offered.is_none() || !self.has_clipboard_manager() {
            return false;
        }
        self.clip.saving = Some(Instant::now() + SAVE_WAIT + WAIT_FOR_DATA);
        self.clip.save_asked = false;
        true
    }

    /// A program asks for what we offer.
    pub(super) fn selection_request(&mut self, ev: SelectionRequestEvent) {
        // (obsolete programs give no property: the target names it)
        let property = if ev.property == x11rb::NONE { ev.target } else { ev.property };
        let done = if ev.selection != self.atoms.CLIPBOARD
            || self.clip.offered.is_none()
            || ev.time != x11rb::CURRENT_TIME && self.clip.offered.as_ref().is_some_and(|o| ev.time < o.since)
        {
            false
        } else if ev.target == self.atoms.MULTIPLE {
            self.convert_multiple(ev.requestor, property)
        } else {
            match self.convert(ev.requestor, ev.target, property) {
                Converted::Done => true,
                Converted::Refused => false,
                Converted::Later => {
                    self.clip.waiting.push((ev, Instant::now() + WAIT_FOR_DATA));
                    return;
                }
            }
        };
        self.answer(&ev, if done { property } else { x11rb::NONE });
    }

    fn answer(&self, ev: &SelectionRequestEvent, property: xproto::Atom) {
        let notify = SelectionNotifyEvent {
            response_type: xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: ev.time,
            requestor: ev.requestor,
            selection: ev.selection,
            target: ev.target,
            property,
        };
        let _ = self.conn.send_event(false, ev.requestor, EventMask::NO_EVENT, notify);
    }

    /// Put what we offer as `target` into `property` on `requestor`.
    fn convert(&mut self, requestor: xproto::Window, target: xproto::Atom, property: xproto::Atom) -> Converted {
        let a = self.atoms;
        let Some(o) = &self.clip.offered else { return Converted::Refused };
        if target == a.TARGETS {
            let mut list = vec![a.TARGETS, a.TIMESTAMP, a.MULTIPLE];
            list.extend(o.mimes.iter().map(|m| m.0));
            let _ = self.conn.change_property32(PropMode::REPLACE, requestor, property, AtomEnum::ATOM, &list);
            return Converted::Done;
        }
        if target == a.TIMESTAMP {
            let _ = self.conn.change_property32(PropMode::REPLACE, requestor, property, AtomEnum::INTEGER, &[o.since]);
            return Converted::Done;
        }
        let Some((_, mime)) = o.mimes.iter().find(|m| m.0 == target) else { return Converted::Refused };
        let data = match o.extra.iter().find(|e| &e.0 == mime) {
            Some((_, d)) => d.clone(),
            None => match o.slot.get() {
                Some(d) => d,
                None if o.slot.coming() => return Converted::Later,
                None => return Converted::Refused,
            },
        };
        let (kind, data) = if target == u32::from(AtomEnum::STRING) {
            (target, Arc::new(latin1(&data)))
        } else if mime == "TEXT" {
            (a.UTF8_STRING, data)
        } else {
            (target, data)
        };
        self.send_data(requestor, property, kind, data);
        Converted::Done
    }

    /// MULTIPLE: each (target, property) pair of the list in `property`,
    /// those that can't be had set to None.
    fn convert_multiple(&mut self, requestor: xproto::Window, property: xproto::Atom) -> bool {
        let reply = self.conn.get_property(false, requestor, property, AtomEnum::ANY, 0, 1024).ok().and_then(|c| c.reply().ok());
        let Some(mut pairs) = reply.and_then(|r| r.value32().map(|v| v.collect::<Vec<u32>>())) else { return false };
        for i in (0..pairs.len() / 2).map(|i| i * 2) {
            if !matches!(self.convert(requestor, pairs[i], pairs[i + 1]), Converted::Done) {
                pairs[i + 1] = x11rb::NONE;
            }
        }
        let _ = self.conn.change_property32(PropMode::REPLACE, requestor, property, self.atoms.ATOM_PAIR, &pairs);
        true
    }

    /// `data` into `property` on `requestor`: at once, or in pieces.
    fn send_data(&mut self, requestor: xproto::Window, property: xproto::Atom, kind: xproto::Atom, data: Arc<Vec<u8>>) {
        if data.len() <= CHUNK {
            let _ = self.conn.change_property8(PropMode::REPLACE, requestor, property, kind, &data);
            return;
        }
        // INCR: the size first; a piece each time the requestor has read the last.
        let _ = self.conn.change_window_attributes(requestor, &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE));
        let _ = self.conn.change_property32(PropMode::REPLACE, requestor, property, self.atoms.INCR, &[data.len() as u32]);
        self.clip.sending.retain(|t| !(t.requestor == requestor && t.property == property));
        self.clip.sending.push(Incr { requestor, property, kind, data, at: 0, until: Instant::now() + STALL });
    }

    pub(super) fn selection_clear(&mut self, ev: SelectionClearEvent) {
        if ev.selection == self.atoms.CLIPBOARD && ev.owner == self.helper {
            // (another program took the clipboard)
            self.clip.offered = None;
            self.refuse_waiting();
        }
    }

    pub(super) fn property_notify(&mut self, ev: PropertyNotifyEvent) {
        if ev.state == xproto::Property::DELETE {
            // A piece was read: the next one (an empty one ends it).
            let Some(i) = self.clip.sending.iter().position(|t| t.requestor == ev.window && t.property == ev.atom) else { return };
            let t = &mut self.clip.sending[i];
            let end = (t.at + CHUNK).min(t.data.len());
            let _ = self.conn.change_property8(PropMode::REPLACE, t.requestor, t.property, t.kind, &t.data[t.at..end]);
            if t.at == end {
                self.clip.sending.remove(i);
            } else {
                t.at = end;
                t.until = Instant::now() + STALL;
            }
        } else if ev.window == self.helper && ev.atom == self.atoms.FLATSHOT_PASTE && self.clip.paste.as_ref().is_some_and(|p| p.incr) {
            self.read_paste();
        }
    }

    pub(super) fn selection_notify(&mut self, ev: SelectionNotifyEvent) {
        if ev.requestor != self.helper {
            return;
        }
        if ev.selection == self.atoms.CLIPBOARD_MANAGER {
            // (saved, or not: then held on to as without a manager)
            self.clip.saved = ev.property != x11rb::NONE;
            self.clip.saving = None;
            return;
        }
        if ev.selection != self.atoms.CLIPBOARD || self.clip.paste.is_none() {
            return;
        }
        if ev.property == x11rb::NONE {
            // Not as this type: the next one.
            let next = self.clip.paste.as_mut().and_then(|p| (!p.types.is_empty()).then(|| p.types.remove(0)));
            match next {
                Some(t) => self.ask_paste(t),
                None => self.end_paste(None),
            }
            return;
        }
        self.read_paste();
    }

    /// Read (and delete) the paste's property: all of it, or the next piece.
    fn read_paste(&mut self) {
        let property = self.atoms.FLATSHOT_PASTE;
        let max = self.clip.paste.as_ref().map_or(0, |p| p.max);
        let reply = self.conn.get_property(true, self.helper, property, AtomEnum::ANY, 0, (max / 4 + 1) as u32).ok().and_then(|c| c.reply().ok());
        let Some(r) = reply else { return self.end_paste(None) };
        let Some(p) = self.clip.paste.as_mut() else { return };
        if !p.incr && r.type_ == self.atoms.INCR {
            p.incr = true; // (the pieces follow, each announced by a new value)
            return;
        }
        if r.bytes_after > 0 || p.got.len() + r.value.len() > p.max {
            return self.end_paste(None);
        }
        p.kind = r.type_;
        p.got.extend_from_slice(&r.value);
        if p.incr && !r.value.is_empty() {
            p.until = Instant::now() + STALL;
            return;
        }
        let got = std::mem::take(&mut p.got);
        self.end_paste(Some(got));
    }

    fn ask_paste(&mut self, kind: xproto::Atom) {
        if let Some(p) = &mut self.clip.paste {
            p.asked = kind;
        }
        let _ = self.conn.convert_selection(self.helper, self.atoms.CLIPBOARD, kind, self.atoms.FLATSHOT_PASTE, self.time);
        let _ = self.conn.flush();
    }

    /// The paste is over, with what came of it (None: nothing).
    fn end_paste(&mut self, got: Option<Vec<u8>>) {
        let Some(p) = self.clip.paste.take() else { return };
        match p.sink {
            PasteSink::Text(tx) => {
                let latin = p.kind == u32::from(AtomEnum::STRING);
                let text = got.map(|g| if latin { g.iter().map(|&b| b as char).collect() } else { String::from_utf8_lossy(&g).into_owned() });
                let _ = tx.send(text);
            }
            PasteSink::Bytes(tx) => {
                let name = self.atom_name(p.asked).unwrap_or_default();
                let _ = tx.send(got.map(|g| (name, g)));
            }
        }
        (p.wake)();
    }

    pub fn paste_text(&mut self, wake: Waker) -> mpsc::Receiver<Option<String>> {
        let (tx, rx) = mpsc::channel();
        self.start_paste(PasteSink::Text(tx), &PASTE_TYPES, MAX_PASTE as usize, wake);
        rx
    }

    /// What's on the clipboard as the first of `mimes` it's offered as (at
    /// most `max` bytes): the type and the data.
    pub fn paste(&mut self, mimes: &[&str], max: u64, wake: Waker) -> crate::clipboard::Pasted {
        let (tx, rx) = mpsc::channel();
        self.start_paste(PasteSink::Bytes(tx), mimes, max as usize, wake);
        rx
    }

    fn start_paste(&mut self, sink: PasteSink, names: &[&str], max: usize, wake: Waker) {
        self.end_paste(None);
        let mut types: Vec<xproto::Atom> = names.iter().filter_map(|t| self.atom(t)).collect();
        let paste = Paste {
            sink,
            wake,
            types: Vec::new(),
            asked: x11rb::NONE,
            kind: x11rb::NONE,
            got: Vec::new(),
            max,
            incr: false,
            until: Instant::now() + WAIT_FOR_DATA,
        };
        self.clip.paste = Some(paste);
        if types.is_empty() {
            return self.end_paste(None);
        }
        let first = types.remove(0);
        if let Some(p) = &mut self.clip.paste {
            p.types = types;
        }
        self.ask_paste(first);
    }

    fn refuse_waiting(&mut self) {
        for (ev, _) in std::mem::take(&mut self.clip.waiting) {
            self.answer(&ev, x11rb::NONE);
        }
    }

    /// Requests whose data has come (or never will), transfers gone quiet,
    /// a paste unanswered, the clipboard manager.
    pub(super) fn clipboard_tick(&mut self) {
        let now = Instant::now();
        if !self.clip.waiting.is_empty() {
            for (ev, until) in std::mem::take(&mut self.clip.waiting) {
                let property = if ev.property == x11rb::NONE { ev.target } else { ev.property };
                match self.convert(ev.requestor, ev.target, property) {
                    Converted::Done => self.answer(&ev, property),
                    Converted::Later if now < until => self.clip.waiting.push((ev, until)),
                    _ => self.answer(&ev, x11rb::NONE),
                }
            }
        }
        self.clip.sending.retain(|t| now < t.until);
        if self.clip.paste.as_ref().is_some_and(|p| now >= p.until) {
            self.end_paste(None);
        }
        if let Some(until) = self.clip.saving {
            let ready = self.clip.offered.as_ref().is_some_and(|o| !o.slot.coming());
            if now >= until {
                self.clip.saving = None;
            } else if ready && !self.clip.save_asked {
                // The types to keep, then the manager is asked to keep them.
                let targets: Vec<u32> = self.clip.offered.as_ref().map(|o| o.mimes.iter().map(|m| m.0).collect()).unwrap_or_default();
                let property = self.atoms.FLATSHOT_SAVE;
                let _ = self.conn.change_property32(PropMode::REPLACE, self.helper, property, AtomEnum::ATOM, &targets);
                let _ = self.conn.convert_selection(self.helper, self.atoms.CLIPBOARD_MANAGER, self.atoms.SAVE_TARGETS, property, self.time);
                self.clip.save_asked = true;
                self.clip.saving = Some(now + SAVE_WAIT);
            }
        }
    }

    /// When the clipboard wants the loop back.
    pub(super) fn clipboard_deadline(&self) -> Option<Instant> {
        let waiting = self.clip.waiting.iter().map(|w| w.1).min();
        let sending = self.clip.sending.iter().map(|t| t.until).min();
        let paste = self.clip.paste.as_ref().map(|p| p.until);
        // (data still being made is checked often)
        let soon = (!self.clip.waiting.is_empty() || self.clip.saving.is_some() && !self.clip.save_asked).then(|| Instant::now() + Duration::from_millis(20));
        [waiting, sending, paste, self.clip.saving, soon].into_iter().flatten().min()
    }

    /// The name of a type, for a program asking in the log.
    pub(super) fn describe(&mut self, atom: xproto::Atom) -> String {
        self.atom_name(atom).unwrap_or_else(|| atom.to_string())
    }
}

/// UTF-8 as Latin-1 (STRING), what doesn't fit as '?'.
fn latin1(utf8: &[u8]) -> Vec<u8> {
    String::from_utf8_lossy(utf8).chars().map(|c| if (c as u32) < 256 { c as u8 } else { b'?' }).collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn latin1() {
        assert_eq!(super::latin1("café ☕".as_bytes()), b"caf\xe9 ?");
    }
}
