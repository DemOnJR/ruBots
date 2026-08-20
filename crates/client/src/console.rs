//! Pacing the console commands the bot sends.
//!
//! Buying, switching weapons and joining a team are all `clc_stringcmd`s on the
//! reliable channel, and the reliable channel is the one thing on this
//! connection that can be overrun. The server copies the **whole** of
//! `netchan.message` into `reliable_buf` in one go, and only when the previous
//! reliable has been acknowledged (`Netchan_Transmit`), so everything the game
//! queues while an acknowledgement is outstanding piles into a single buffer.
//! Overflow it and the client is dropped with `"Reliable channel overflowed"`.
//!
//! Each buy alias is cheap outbound but expensive inbound: the server answers
//! with `Money`, `WeapPickup`, `AmmoX`, `CurWeapon`, `StatusIcon` and more. Nine
//! aliases fired back to back is a burst of reliable traffic in both
//! directions. So commands leave one at a time, spaced, and only while nothing
//! is already in flight.
//!
//! ReGameDLL itself rate-limits almost nothing here — buy aliases, `jointeam`,
//! `joinclass` and `weapon_*` have no cooldown at all (`client.cpp:2654-3730`).
//! The limits that do exist are on chat (`say` 0.3 s plus a hard 0.66 s inside
//! `Host_Say`), `fullupdate` (0.6 s) and radio. This queue is about not
//! drowning the channel, not about appeasing a server-side limiter.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Minimum gap between two console commands.
///
/// A real client's join sequence was measured at roughly 180 ms between
/// commands (`jointeam` 3.17 s, `joinclass` 3.33 s, `specmode` 3.43 s,
/// `VModEnable` 3.58 s), so this is the same order.
pub const SPACING: Duration = Duration::from_millis(150);

/// `say` is rate-limited server-side to 0.3 s per client, and `Host_Say`
/// applies a further 0.66 s (`client.cpp:2662-2674`, `client.cpp:764-767`).
/// Exceeding it is silently dropped, so a bot that chats faster simply loses
/// the messages.
pub const SAY_SPACING: Duration = Duration::from_millis(700);

/// A console command waiting to go out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub text: String,
    /// Chat is throttled harder than everything else.
    pub is_say: bool,
}

/// FIFO queue that releases at most one command per [`SPACING`].
#[derive(Debug, Default)]
pub struct ConsoleQueue {
    queue: VecDeque<Pending>,
    last_sent: Option<Instant>,
    last_say: Option<Instant>,
    pub sent: u32,
}

impl ConsoleQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a command, unless the same one is already waiting.
    ///
    /// De-duplication is not a nicety here, it is load-bearing. The bot's
    /// think() runs every frame and re-emits its whole buy plan for as long as
    /// the freeze period lasts, so a plain FIFO accumulates hundreds of
    /// identical aliases: measured, 153 queued commands draining at the 150 ms
    /// spacing, i.e. about 25 seconds of backlog. The purchases then arrive
    /// long after the bot has left the buy zone and every one of them fails.
    ///
    /// Only the PENDING set is deduplicated. A command that has already gone
    /// out can be sent again -- switching back to a weapon, or re-buying next
    /// round -- which is why this is not a "sent once ever" set.
    pub fn push(&mut self, text: impl Into<String>) {
        let text = text.into();
        if self.queue.iter().any(|p| p.text == text) {
            return;
        }
        self.queue.push_back(Pending {
            text,
            is_say: false,
        });
    }

    pub fn say(&mut self, text: impl Into<String>) {
        self.queue.push_back(Pending {
            text: format!("say {}", text.into()),
            is_say: true,
        });
    }

    pub fn extend<I, S>(&mut self, items: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        for t in items {
            self.push(t);
        }
    }

    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// The command at the head of the queue, for tracing.
    ///
    /// A queue that is not draining looks identical from the outside to one
    /// that is draining and being refilled -- the count is the same. The only
    /// way to tell is to see what is actually stuck at the front.
    pub fn peek(&self) -> Option<&str> {
        self.queue.front().map(|p| p.text.as_str())
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub fn clear(&mut self) {
        self.queue.clear();
    }

    /// The next command to send, if one is due.
    ///
    /// `settled` is whether the reliable channel is idle — pass
    /// `Session::reliables_settled()`. Holding off while a reliable is in
    /// flight is what keeps a burst from accumulating in the server's single
    /// reliable buffer.
    pub fn next(&mut self, now: Instant, settled: bool) -> Option<String> {
        if !settled || self.queue.is_empty() {
            return None;
        }
        if let Some(last) = self.last_sent {
            if now.saturating_duration_since(last) < SPACING {
                return None;
            }
        }
        // Chat has its own, longer cooldown; if the head is a `say` that is not
        // due yet, wait rather than reordering -- ordering matters for
        // jointeam/joinclass, so the queue never overtakes itself.
        if self.queue.front().is_some_and(|p| p.is_say) {
            if let Some(last) = self.last_say {
                if now.saturating_duration_since(last) < SAY_SPACING {
                    return None;
                }
            }
        }
        let item = self.queue.pop_front()?;
        self.last_sent = Some(now);
        if item.is_say {
            self.last_say = Some(now);
        }
        self.sent += 1;
        Some(item.text)
    }
}

/// What to buy, in priority order.
///
/// Buy **aliases**, never `weapon_<name>` — that is `SelectItem`, a weapon
/// switch, and buying with it silently does nothing (`client.cpp:3560-3563`
/// versus the alias table at `client.cpp:2465-2606`).
pub fn buy_plan(money: i32, is_ct: bool) -> Vec<&'static str> {
    let primary = if is_ct { "m4a1" } else { "ak47" };
    // Prices from `regamedll/dlls/weapontype.cpp`: m4a1 3100, ak47 2500,
    // vesthelm 1000, defuser 200, hegren 300, flash 200, deagle 650, vest 650.
    if money >= 4000 {
        let mut v = vec![primary, "primammo", "vesthelm", "hegren", "flash"];
        if is_ct {
            v.push("defuser");
        }
        v.push("secammo");
        v
    } else if money >= 2500 {
        let mut v = vec![primary, "primammo", "vest"];
        if is_ct {
            v.push("defuser");
        }
        v
    } else if money >= 1000 {
        vec!["deagle", "secammo", "vest"]
    } else {
        vec!["secammo"]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(q: &mut ConsoleQueue, start: Instant, steps: u32, step: Duration) -> Vec<String> {
        let mut out = Vec::new();
        for i in 0..steps {
            let now = start + step * i;
            if let Some(c) = q.next(now, true) {
                out.push(c);
            }
        }
        out
    }

    /// The bot re-emits its buy plan every frame while frozen. Without
    /// de-duplication that is a self-inflicted denial of service: the queue
    /// grows faster than the 150 ms spacing drains it, and by the time an
    /// alias reaches the server the bot is nowhere near a buy zone.
    #[test]
    fn re_emitting_the_same_plan_every_frame_does_not_pile_up() {
        let mut q = ConsoleQueue::new();
        for _ in 0..200 {
            q.extend(["vesthelm", "ak47", "primammo", "hegren"]);
        }
        assert_eq!(q.len(), 4, "queued {} copies of a 4-item plan", q.len());
    }

    /// ...but a command that has already been sent may be sent again.
    #[test]
    fn a_command_can_be_requeued_once_it_has_gone_out() {
        let start = Instant::now();
        let mut q = ConsoleQueue::new();
        q.push("weapon_knife");
        assert_eq!(q.next(start, true).as_deref(), Some("weapon_knife"));
        q.push("weapon_knife");
        assert_eq!(q.len(), 1, "a sent command must be re-sendable");
    }

    #[test]
    fn commands_leave_one_at_a_time_and_in_order() {
        let start = Instant::now();
        let mut q = ConsoleQueue::new();
        q.extend(["jointeam 1", "joinclass 6", "ak47"]);

        let got = drain(&mut q, start, 40, Duration::from_millis(50));
        assert_eq!(got, vec!["jointeam 1", "joinclass 6", "ak47"]);
    }

    /// Ordering is not a nicety: `joinclass` is refused outright unless
    /// `jointeam` has already moved `m_iMenu` (`client.cpp:3436-3441`).
    #[test]
    fn a_throttled_say_does_not_let_later_commands_overtake_it() {
        let start = Instant::now();
        let mut q = ConsoleQueue::new();
        q.say("hello");
        q.say("again");
        q.push("jointeam 1");

        let got = drain(&mut q, start, 60, Duration::from_millis(50));
        assert_eq!(got, vec!["say hello", "say again", "jointeam 1"]);
    }

    #[test]
    fn nothing_is_sent_while_a_reliable_is_in_flight() {
        let start = Instant::now();
        let mut q = ConsoleQueue::new();
        q.push("ak47");
        assert_eq!(q.next(start, false), None, "sent while unsettled");
        assert_eq!(q.next(start, true).as_deref(), Some("ak47"));
    }

    #[test]
    fn commands_are_spaced_by_at_least_the_configured_gap() {
        let start = Instant::now();
        let mut q = ConsoleQueue::new();
        q.extend(["a", "b"]);
        assert!(q.next(start, true).is_some());
        assert_eq!(q.next(start + Duration::from_millis(10), true), None);
        assert_eq!(
            q.next(start + SPACING, true).as_deref(),
            Some("b"),
            "second command should be due after exactly SPACING"
        );
    }

    #[test]
    fn chat_waits_for_its_longer_cooldown() {
        let start = Instant::now();
        let mut q = ConsoleQueue::new();
        q.say("one");
        q.say("two");
        assert!(q.next(start, true).is_some());
        // Past the general spacing but not past the chat cooldown.
        assert_eq!(q.next(start + SPACING, true), None);
        assert!(q.next(start + SAY_SPACING, true).is_some());
    }

    #[test]
    fn the_buy_plan_never_uses_a_weapon_switch_as_a_purchase() {
        for money in [0, 800, 1500, 3000, 8000] {
            for ct in [false, true] {
                for alias in buy_plan(money, ct) {
                    assert!(
                        !alias.starts_with("weapon_"),
                        "`{alias}` is SelectItem, not a purchase"
                    );
                    assert!(!alias.is_empty());
                }
            }
        }
    }

    #[test]
    fn the_buy_plan_degrades_with_money_and_only_cts_buy_a_defuser() {
        assert!(buy_plan(8000, false).contains(&"ak47"));
        assert!(buy_plan(8000, true).contains(&"m4a1"));
        assert!(buy_plan(8000, true).contains(&"defuser"));
        assert!(
            !buy_plan(8000, false).contains(&"defuser"),
            "a terrorist buying a defuse kit is refused with #Alias_Not_Avail"
        );
        // Broke: still top up ammo rather than emitting nothing.
        assert!(!buy_plan(0, false).is_empty());
    }
}
