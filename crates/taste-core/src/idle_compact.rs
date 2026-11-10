//! Compacting a conversation the user has stepped away from, while its
//! prompt cache is still warm.
//!
//! A conversation's prompt cache lasts an hour after its last request (on
//! a Claude subscription; an API key's lasts five minutes). The message a
//! user sends after a longer break pays to read the whole conversation
//! again, uncached. Compacting it just before the hour is up reads it once
//! more from the cache, and the next message sends the summary instead.
//! Claude Code does this itself in its terminal, its VS Code panel, and the
//! desktop app (its `idleCompaction` setting), and not under the Agent SDK,
//! which is how the IDE runs it. So the IDE does it, by sending the
//! agent's own `/compact` when a chat has been quiet long enough, under the
//! same conditions Claude Code documents: on the one-hour cache, past
//! 200,000 tokens of context, and about an hour idle (David, 2026-11-10:
//! "Can you add this idle compaction feature to the IDE?").
//!
//! This module is the decision, with no clock and no GTK of its own;
//! `taste-app`'s chat pane gathers the [`Facts`] and acts on the
//! [`Verdict`].

use std::time::Duration;

/// How long after a conversation's last request it is compacted: five
/// minutes inside the cache's hour, so the compaction's own request still
/// reads the conversation from the cache.
pub const AFTER: Duration = Duration::from_secs(55 * 60);

/// How long a conversation's prompt cache lasts after its last request,
/// on the one-hour TTL. Past it, compacting saves nothing: the compaction
/// reads the whole conversation uncached, which is what the next message
/// would have done anyway.
pub const CACHE_LIFETIME: Duration = Duration::from_secs(60 * 60);

/// The context a conversation must hold before it is worth compacting:
/// Claude Code's own default threshold for idle compaction.
pub const MIN_TOKENS: u64 = 200_000;

/// The agent command that compacts, as the agent advertises it (without
/// its slash).
pub const COMMAND: &str = "compact";

/// What the decision is made from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Facts {
    /// The chat's switch for this ("Compact while away").
    pub enabled: bool,
    /// The agent advertises [`COMMAND`].
    pub advertised: bool,
    /// The conversation is on the one-hour cache: a Claude subscription,
    /// within its plan, rather than an API key or another upstream.
    pub hour_cache: bool,
    /// Tokens in the conversation's context now.
    pub context: u64,
    /// Wall-clock time since the conversation's last request, if it has
    /// made one. Wall clock and not monotonic, because a machine asleep
    /// stops a monotonic clock and does not stop the cache's hour.
    pub idle: Option<Duration>,
    /// Nothing is under way: no turn running, nothing queued or held for
    /// the agent, and no question waiting on the user.
    pub quiet: bool,
}

/// What to do about a conversation now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Compact it now.
    Compact,
    /// Ask again after this long.
    Wait(Duration),
    /// Not this conversation, not now. Whatever changes the answer (a turn
    /// ending, the switch turned back on) asks again.
    Skip(Skip),
}

/// Why a conversation is not compacted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    Off,
    NoCommand,
    ShortCache,
    Small,
    NoRequest,
    Busy,
    Expired,
}

/// Whether to compact a conversation, from what is known about it.
pub fn verdict(facts: &Facts) -> Verdict {
    if !facts.enabled {
        return Verdict::Skip(Skip::Off);
    }
    if !facts.advertised {
        return Verdict::Skip(Skip::NoCommand);
    }
    if !facts.hour_cache {
        return Verdict::Skip(Skip::ShortCache);
    }
    let Some(idle) = facts.idle else {
        return Verdict::Skip(Skip::NoRequest);
    };
    if idle >= CACHE_LIFETIME {
        return Verdict::Skip(Skip::Expired);
    }
    if idle < AFTER {
        // Asked again when the time comes, whatever the size now: the
        // usage that says how full the context is can land after the turn
        // that filled it.
        return Verdict::Wait(AFTER - idle);
    }
    if facts.context < MIN_TOKENS {
        return Verdict::Skip(Skip::Small);
    }
    if !facts.quiet {
        return Verdict::Skip(Skip::Busy);
    }
    Verdict::Compact
}

#[cfg(test)]
mod tests {
    use super::*;

    fn due() -> Facts {
        Facts {
            enabled: true,
            advertised: true,
            hour_cache: true,
            context: 412_000,
            idle: Some(AFTER),
            quiet: true,
        }
    }

    #[test]
    fn a_large_quiet_conversation_is_compacted_before_the_hour() {
        assert_eq!(verdict(&due()), Verdict::Compact);
        let late = Facts {
            idle: Some(CACHE_LIFETIME - Duration::from_secs(1)),
            ..due()
        };
        assert_eq!(verdict(&late), Verdict::Compact);
    }

    #[test]
    fn an_early_ask_waits_out_the_rest() {
        let early = Facts {
            idle: Some(Duration::from_secs(5 * 60)),
            ..due()
        };
        assert_eq!(verdict(&early), Verdict::Wait(Duration::from_secs(50 * 60)));
        // Small now is not small then: the size is read when it is due.
        let small = Facts {
            context: 10_000,
            ..early
        };
        assert_eq!(verdict(&small), Verdict::Wait(Duration::from_secs(50 * 60)));
    }

    /// A machine that slept through the hour wakes to a cold cache, and
    /// compacting then reads the whole conversation uncached.
    #[test]
    fn past_the_hour_it_saves_nothing() {
        let slept = Facts {
            idle: Some(CACHE_LIFETIME),
            ..due()
        };
        assert_eq!(verdict(&slept), Verdict::Skip(Skip::Expired));
    }

    #[test]
    fn each_condition_holds_it_back() {
        let cases = [
            (
                Facts {
                    enabled: false,
                    ..due()
                },
                Skip::Off,
            ),
            (
                Facts {
                    advertised: false,
                    ..due()
                },
                Skip::NoCommand,
            ),
            (
                Facts {
                    hour_cache: false,
                    ..due()
                },
                Skip::ShortCache,
            ),
            (
                Facts {
                    context: MIN_TOKENS - 1,
                    ..due()
                },
                Skip::Small,
            ),
            (
                Facts {
                    idle: None,
                    ..due()
                },
                Skip::NoRequest,
            ),
            (
                Facts {
                    quiet: false,
                    ..due()
                },
                Skip::Busy,
            ),
        ];
        for (facts, why) in cases {
            assert_eq!(verdict(&facts), Verdict::Skip(why), "{facts:?}");
        }
        let at_threshold = Facts {
            context: MIN_TOKENS,
            ..due()
        };
        assert_eq!(verdict(&at_threshold), Verdict::Compact);
    }
}
