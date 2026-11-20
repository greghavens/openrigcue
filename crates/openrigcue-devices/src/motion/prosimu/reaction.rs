//! The PRS200 "reaction speed" profile: which speed/acceleration bytes the boards get and when.
//!
//! Two variants exist: [`Style::Classic`] and [`Style::Sv`]. Both compare a target pair with the pair
//! last sent and send when they differ; they differ in how the target follows the game state.

use std::time::Duration;

use crate::motion::clock::Clock;
use crate::motion::driver_support::{map_clamped, py_int};

/// The clamped rescale of `x` from `in_min..in_max` to `out_min..out_max`, truncated to an integer.
fn map_double(x: f64, in_min: i64, in_max: i64, out_min: i64, out_max: i64) -> i64 {
    py_int(map_clamped(x, in_min as f64, in_max as f64, out_min as f64, out_max as f64))
}
use crate::motion::python_print;

/// Delay after a game-state change before the in-game profile applies.
const TRIGGER_DELAY: Duration = Duration::from_secs(3);
/// Start values: target `speed 4 / accel 200`, last sent `80 / 80`.
const INITIAL_TARGET: Pair = Pair { speed: 4, accel: 200 };
const INITIAL_SENT: Pair = Pair { speed: 80, accel: 80 };
/// Number of start-up sends of [`init_reaction_speed`] and the pause after each.
const INIT_SENDS: u32 = 5;
const INIT_PAUSE: Duration = Duration::from_secs(2);
/// The game states of [`game_state`] that are not a game.
pub const IDLE: &str = "none";
pub const TEST: &str = "test";

/// A speed/acceleration byte pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pair {
    pub speed: i64,
    pub accel: i64,
}

/// Which reaction-speed variant runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// The PRS200 Dyadic models, with or without traction-loss surge.
    Classic,
    /// The PRS200 Dyadic SV and PRS50 models; `sv_model` is the SV model-name test.
    Sv { sv_model: bool },
}

/// The game state: `'none'` when idle (empty game or no listener), `'test'` in motion-test mode.
/// Only the drivers that test the listener (`listener_tested`) are idle while no listener runs.
pub fn game_state(game_running: &str, listener_running: &str, motion_test_mode: bool, listener_tested: bool) -> String {
    if motion_test_mode {
        return TEST.into();
    }
    if game_running.is_empty() || (listener_tested && listener_running == IDLE) {
        return IDLE.into();
    }
    game_running.into()
}

/// The in-game pair of the classic variant: the speed is the reaction speed rescaled from 70..100 to
/// 150..255 and truncated, `accel = 255 - speed`.
fn classic_game_pair(reaction_speed: f64) -> Pair {
    let speed = map_double(reaction_speed, 70, 100, 150, 255);
    Pair { speed, accel: 255 - speed }
}

/// The target pair of the SV variant for a game state.
pub fn sv_calc_pair(state: &str, sv_model: bool, reaction_speed: i64) -> Pair {
    match sv_calc_pair_with(state, sv_model, &mut || Ok::<_, std::convert::Infallible>(reaction_speed as f64)) {
        Ok(pair) => pair,
        Err(never) => match never {},
    }
}

fn sv_calc_pair_with<E>(
    state: &str,
    sv_model: bool,
    reaction_speed: &mut impl FnMut() -> Result<f64, E>,
) -> Result<Pair, E> {
    if state == TEST {
        return Ok(Pair { speed: 5, accel: 5 });
    }
    if sv_model {
        let accel = 256 - map_double(reaction_speed()?, 100, 70, 125, 240);
        let speed = 256 - map_double(reaction_speed()?, 100, 70, 125, 240);
        return Ok(Pair { speed, accel });
    }
    let accel = 255 - map_double(reaction_speed()?, 70, 100, 100, 255);
    let speed = map_double(reaction_speed()?, 70, 100, 150, 255);
    Ok(Pair { speed, accel })
}

/// State of the reaction-speed check: the target pair, the pair last sent, the trigger and
/// the previous game state.
#[derive(Debug, Clone)]
pub struct ReactionSpeed {
    target: Pair,
    sent: Pair,
    trigger_start: Option<Duration>,
    previous_state: Option<String>,
}

impl Default for ReactionSpeed {
    fn default() -> Self {
        Self::new()
    }
}

impl ReactionSpeed {
    pub fn new() -> Self {
        ReactionSpeed { target: INITIAL_TARGET, sent: INITIAL_SENT, trigger_start: None, previous_state: None }
    }

    /// One frame of the check: when the target pair differs from the pair last sent, it becomes the
    /// pair last sent and `send` delivers it to the boards. The state is remembered only when that succeeded:
    /// the previous game state is stored after the sends, so a failing send repeats the state-change
    /// handling on the next frame (while the pair, already recorded as sent, is not sent again).
    pub fn check<E>(
        &mut self,
        style: Style,
        state: &str,
        reaction_speed: i64,
        now: Duration,
        send: impl FnOnce(Pair) -> Result<(), E>,
    ) -> Result<(), E> {
        self.check_with(style, state, || Ok(reaction_speed as f64), now, send)
    }

    pub(crate) fn check_with<E>(
        &mut self,
        style: Style,
        state: &str,
        mut reaction_speed: impl FnMut() -> Result<f64, E>,
        now: Duration,
        send: impl FnOnce(Pair) -> Result<(), E>,
    ) -> Result<(), E> {
        match style {
            Style::Classic => self.check_classic(state, &mut reaction_speed, now)?,
            Style::Sv { sv_model } => self.check_sv(state, sv_model, &mut reaction_speed, now)?,
        }
        if self.target != self.sent {
            self.sent = self.target;
            send(self.sent)?;
        }
        self.previous_state = Some(state.to_string());
        Ok(())
    }

    fn state_changed(&self, state: &str) -> bool {
        self.previous_state.as_deref() != Some(state)
    }

    fn trigger_elapsed(&self, now: Duration) -> bool {
        self.trigger_start.is_some_and(|start| now.saturating_sub(start) > TRIGGER_DELAY)
    }

    /// The classic variant of the check.
    fn check_classic<E>(
        &mut self,
        state: &str,
        reaction_speed: &mut impl FnMut() -> Result<f64, E>,
        now: Duration,
    ) -> Result<(), E> {
        if self.state_changed(state) {
            if state != IDLE {
                self.trigger_start = Some(now);
            } else {
                self.target = INITIAL_TARGET;
            }
        }
        if self.trigger_elapsed(now) {
            if state != IDLE {
                // The acceleration is assigned before the separate speed lookup.
                self.target.accel = classic_game_pair(reaction_speed()?).accel;
                self.target.speed = classic_game_pair(reaction_speed()?).speed;
            }
            self.trigger_start = None;
        }
        Ok(())
    }

    /// The SV variant of the check: the last-sent pair is reset on a state
    /// change, a slow profile holds for 3 s, then [`sv_calc_pair`] is followed on every frame.
    fn check_sv<E>(
        &mut self,
        state: &str,
        sv_model: bool,
        reaction_speed: &mut impl FnMut() -> Result<f64, E>,
        now: Duration,
    ) -> Result<(), E> {
        if self.state_changed(state) {
            self.trigger_start = Some(now);
            self.sent = if sv_model { Pair { speed: 6, accel: 6 } } else { Pair { speed: 5, accel: 199 } };
        }
        match self.trigger_start {
            Some(_) if self.trigger_elapsed(now) => self.trigger_start = None,
            Some(_) => {
                self.target = if sv_model { Pair { speed: 5, accel: 5 } } else { INITIAL_TARGET };
            }
            None => self.target = sv_calc_pair_with(state, sv_model, reaction_speed)?,
        }
        Ok(())
    }
}

/// The pair `init_reaction_speed` sends five times.
fn initial_pair(style: Style) -> Pair {
    match style {
        Style::Sv { sv_model: true } => Pair { speed: 5, accel: 5 },
        _ => INITIAL_TARGET,
    }
}

/// The in-game pair `init_reaction_speed` sends once after the five start-up sends.
fn game_pair(style: Style, reaction_speed: i64) -> Pair {
    match game_pair_with(style, &mut || Ok::<_, std::convert::Infallible>(reaction_speed as f64)) {
        Ok(pair) => pair,
        Err(never) => match never {},
    }
}

fn game_pair_with<E>(style: Style, reaction_speed: &mut impl FnMut() -> Result<f64, E>) -> Result<Pair, E> {
    match style {
        Style::Sv { sv_model: true } => {
            let accel = 256 - map_double(reaction_speed()?, 70, 100, 125, 240);
            let speed = 256 - map_double(reaction_speed()?, 70, 100, 125, 240);
            Ok(Pair { speed, accel })
        }
        _ => {
            let accel = 255 - map_double(reaction_speed()?, 70, 100, 100, 255);
            let speed = map_double(reaction_speed()?, 70, 100, 150, 255);
            Ok(Pair { speed, accel })
        }
    }
}

/// The body of the start-up thread that a connect starts:
/// five times the initial pair, each send followed by a pause of two seconds; then,
/// when the game state is not `'none'`, the in-game pair. `current` gives the game state and
/// `[MOTIONBOX] reaction_speed` as they are when the five sends are done.
pub fn init_reaction_speed(
    style: Style,
    clock: &mut dyn Clock,
    current: impl FnOnce() -> (String, i64),
    mut send: impl FnMut(Pair),
) {
    initial_sends(style, clock, &mut send);
    let (state, reaction_speed) = current();
    if state != IDLE {
        let pair = game_pair(style, reaction_speed);
        say_velocity(INIT_SENDS, pair);
        send(pair);
    }
}

pub fn init_reaction_speed_checked<E>(
    style: Style,
    clock: &mut dyn Clock,
    current: impl FnOnce() -> String,
    reaction_speed: impl FnMut() -> Result<f64, E>,
    send: impl FnMut(Pair),
) -> Result<(), E> {
    init_reaction_speed_live(clock, current, || Ok(style), reaction_speed, send)
}

pub(crate) fn init_reaction_speed_live<E>(
    clock: &mut dyn Clock,
    current: impl FnOnce() -> String,
    mut style: impl FnMut() -> Result<Style, E>,
    mut reaction_speed: impl FnMut() -> Result<f64, E>,
    mut send: impl FnMut(Pair),
) -> Result<(), E> {
    initial_sends(style()?, clock, &mut send);
    if current() != IDLE {
        let pair = game_pair_with(style()?, &mut reaction_speed)?;
        say_velocity(INIT_SENDS, pair);
        send(pair);
    }
    Ok(())
}

fn say_velocity(i: u32, pair: Pair) {
    python_print(&["velocity changed to", &i.to_string(), &pair.speed.to_string(), &pair.accel.to_string()]);
}

fn initial_sends(style: Style, clock: &mut dyn Clock, send: &mut impl FnMut(Pair)) {
    // Each of the five sends prints its line with its index;
    // the in-game send then prints the same line with the index 5.
    for i in 0..INIT_SENDS {
        let pair = initial_pair(style);
        say_velocity(i, pair);
        send(pair);
        clock.sleep(INIT_PAUSE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    #[test]
    fn native_prs200_startup_model_is_read_again_after_the_initial_sends() {
        let clock = crate::motion::testing::FakeClock::default();
        let mut sleeper = clock.clone();
        let mut styles = [Ok(Style::Sv { sv_model: true }), Ok(Style::Classic)].into_iter();
        let mut sent = Vec::new();
        init_reaction_speed_live(
            &mut sleeper,
            || "game".into(),
            || styles.next().unwrap(),
            || Ok::<f64, &str>(85.0),
            |pair| sent.push(pair),
        )
        .unwrap();
        assert_eq!(&sent[..5], &[Pair { speed: 5, accel: 5 }; 5]);
        assert_eq!(sent[5], Pair { speed: 202, accel: 78 });
        let mut styles = [Ok(Style::Classic), Err("missing model")].into_iter();
        sent.clear();
        assert_eq!(
            init_reaction_speed_live(
                &mut sleeper,
                || "game".into(),
                || styles.next().unwrap(),
                || panic!("no numeric read after a model failure"),
                |pair| sent.push(pair)
            ),
            Err("missing model")
        );
        assert_eq!(sent, vec![INITIAL_TARGET; 5]);
    }

    #[test]
    fn native_prs200_reaction_classic_defers_errors_until_the_profile_read() {
        let mut reaction = ReactionSpeed::new();
        let mut reads = 0;
        for (state, now) in [(IDLE, Duration::ZERO), ("game", S), ("game", S * 4)] {
            reaction
                .check_with(
                    Style::Classic,
                    state,
                    || {
                        reads += 1;
                        Err("bad")
                    },
                    now,
                    |_| Ok(()),
                )
                .unwrap();
        }
        assert_eq!(reads, 0);
        let result = reaction.check_with(
            Style::Classic,
            "game",
            || {
                reads += 1;
                Err("bad")
            },
            S * 5,
            |_| panic!("no send after a failed read"),
        );
        assert_eq!(result, Err("bad"));
        assert_eq!(reads, 1);
        assert_eq!(reaction.target, INITIAL_TARGET);
        assert_eq!(reaction.trigger_start, Some(S));
    }

    #[test]
    fn native_prs200_reaction_second_read_keeps_the_acceleration_prefix() {
        let mut reaction = ReactionSpeed::new();
        check(&mut reaction, Style::Classic, IDLE, 85, Duration::ZERO);
        check(&mut reaction, Style::Classic, "game", 85, S);
        let mut values = [Ok(85.0), Err("second read")].into_iter();
        let result = reaction.check_with(
            Style::Classic,
            "game",
            || values.next().unwrap(),
            S * 5,
            |_| panic!("no send after a failed second read"),
        );
        assert_eq!(result, Err("second read"));
        assert_eq!(reaction.target, Pair { speed: 4, accel: 53 });
        assert_eq!(reaction.sent, INITIAL_TARGET);
        assert_eq!(reaction.trigger_start, Some(S));
    }

    #[test]
    fn native_prs200_reaction_sv_skips_the_read_in_test_mode() {
        for sv_model in [false, true] {
            let mut reaction = ReactionSpeed::new();
            let style = Style::Sv { sv_model };
            for now in [Duration::ZERO, S * 4, S * 5] {
                reaction.check_with(style, TEST, || Err("must be skipped"), now, |_| Ok(())).unwrap();
            }
            assert_eq!(reaction.target, Pair { speed: 5, accel: 5 });
            reaction.check_with(style, "game", || Err("not yet"), S * 6, |_| Ok(())).unwrap();
            reaction.check_with(style, "game", || Err("not yet"), S * 10, |_| Ok(())).unwrap();
            assert_eq!(reaction.check_with(style, "game", || Err("bad"), S * 11, |_| Ok(())), Err("bad"));
        }
    }

    #[test]
    fn native_prs200_reaction_startup_keeps_five_sends_before_a_game_read_failure() {
        for style in [Style::Classic, Style::Sv { sv_model: false }, Style::Sv { sv_model: true }] {
            let clock = crate::motion::testing::FakeClock::default();
            let mut sleeper = clock.clone();
            let mut sent = Vec::new();
            let result = init_reaction_speed_checked(
                style,
                &mut sleeper,
                || "game".into(),
                || Err("bad"),
                |pair| sent.push(pair),
            );
            assert_eq!(result, Err("bad"));
            assert_eq!(sent, vec![initial_pair(style); 5]);
            assert_eq!(clock.now(), S * 10);
            init_reaction_speed_checked(
                style,
                &mut sleeper,
                || IDLE.into(),
                || Err::<f64, _>("must be skipped"),
                |_| {},
            )
            .unwrap();
            assert_eq!(clock.now(), S * 20);
        }
    }

    #[test]
    fn native_prs200_reaction_sv_reads_each_operand_separately() {
        for (sv_model, expected) in [(false, Pair { speed: 255, accel: 155 }), (true, Pair { speed: 131, accel: 16 })] {
            let style = Style::Sv { sv_model };
            let mut reaction = ReactionSpeed::new();
            check(&mut reaction, style, "game", 70, Duration::ZERO);
            check(&mut reaction, style, "game", 70, S * 4);
            let mut values = [Ok::<f64, &str>(70.0), Ok(100.0)].into_iter();
            reaction.check_with(style, "game", || values.next().unwrap(), S * 5, |_| Ok::<(), &str>(())).unwrap();
            assert_eq!(reaction.target, expected);
            let mut values = [Ok(85.0), Err("second")].into_iter();
            assert_eq!(
                reaction.check_with(
                    style,
                    "game",
                    || values.next().unwrap(),
                    S * 6,
                    |_| panic!("no send after a failed second operand")
                ),
                Err("second")
            );
            assert_eq!(reaction.target, expected);
            assert_eq!(reaction.sent, expected);
        }
    }

    #[test]
    fn native_prs200_reaction_startup_reads_each_operand_separately() {
        for style in [Style::Classic, Style::Sv { sv_model: false }, Style::Sv { sv_model: true }] {
            let clock = crate::motion::testing::FakeClock::default();
            let mut sleeper = clock.clone();
            let mut sent = Vec::new();
            let mut values = [Ok::<f64, &str>(70.0), Ok(100.0)].into_iter();
            init_reaction_speed_checked(
                style,
                &mut sleeper,
                || "game".into(),
                || values.next().unwrap(),
                |pair| sent.push(pair),
            )
            .unwrap();
            let expected = if style == (Style::Sv { sv_model: true }) {
                Pair { speed: 16, accel: 131 }
            } else {
                Pair { speed: 255, accel: 155 }
            };
            assert_eq!(sent[5], expected);
            let mut values = [Ok(85.0), Err("second")].into_iter();
            sent.clear();
            assert_eq!(
                init_reaction_speed_checked(
                    style,
                    &mut sleeper,
                    || "game".into(),
                    || values.next().unwrap(),
                    |pair| sent.push(pair)
                ),
                Err("second")
            );
            assert_eq!(sent, vec![initial_pair(style); 5]);
        }
    }

    /// One `check` whose sends always succeed; returns the pair sent, if any.
    fn check(r: &mut ReactionSpeed, style: Style, state: &str, rs: i64, now: Duration) -> Option<Pair> {
        let mut sent = None;
        r.check(style, state, rs, now, |pair| {
            sent = Some(pair);
            Ok::<(), ()>(())
        })
        .unwrap();
        sent
    }

    #[test]
    fn get_game_running() {
        // Classic: '' → 'none'; listener 'none' → 'none'; test mode → 'test'.
        assert_eq!(game_state("", "dirt", false, true), "none");
        assert_eq!(game_state("ac", "none", false, true), "none");
        assert_eq!(game_state("ac", "ac", false, true), "ac");
        assert_eq!(game_state("ac", "ac", true, true), "test");
        // The SV and PRS50 drivers do not look at the listener.
        assert_eq!(game_state("ac", "none", false, false), "ac");
        assert_eq!(game_state("", "dirt", false, false), "none");
    }

    #[test]
    fn classic_first_frame_sends_the_slow_profile() {
        const STYLE: Style = Style::Classic;
        // Start: target (4, 200), last sent (80, 80) → first check sends 4/200.
        let mut r = ReactionSpeed::new();
        assert_eq!(check(&mut r, STYLE, "none", 85, Duration::ZERO), Some(Pair { speed: 4, accel: 200 }));
        assert_eq!(check(&mut r, STYLE, "none", 85, S), None);
    }

    #[test]
    fn classic_game_profile_after_three_seconds() {
        const STYLE: Style = Style::Classic;
        // State change to a game → trigger; > 3 s later the target speed is rs rescaled
        // from 70..100 to 150..255 and truncated, the target acceleration 255 - that.
        let mut r = ReactionSpeed::new();
        check(&mut r, STYLE, "none", 100, Duration::ZERO);
        assert_eq!(check(&mut r, STYLE, "ac", 100, S), None);
        assert_eq!(check(&mut r, STYLE, "ac", 100, S * 4), None, "exactly 3 s is not > 3");
        assert_eq!(
            check(&mut r, STYLE, "ac", 100, S * 4 + Duration::from_millis(1)),
            Some(Pair { speed: 255, accel: 0 })
        );
        // Back to idle: the slow profile is targeted at once.
        assert_eq!(check(&mut r, STYLE, "none", 100, S * 10), Some(Pair { speed: 4, accel: 200 }));
    }

    #[test]
    fn failed_send_keeps_the_previous_state() {
        // The pair last sent is assigned before the sends,
        // the previous game state only after them, so a failing send leaves the old state.
        const STYLE: Style = Style::Classic;
        let mut r = ReactionSpeed::new();
        check(&mut r, STYLE, "none", 100, Duration::ZERO);
        check(&mut r, STYLE, "ac", 100, S);
        assert_eq!(check(&mut r, STYLE, "ac", 100, S * 5), Some(Pair { speed: 255, accel: 0 }));
        // Going idle targets (4, 200); the send fails.
        let failed = r.check(STYLE, "none", 100, S * 6, |_| Err::<(), _>("board missing"));
        assert_eq!(failed, Err("board missing"));
        assert_eq!(r.previous_state.as_deref(), Some("ac"));
        // The pair counts as sent and is not repeated; the state is recorded on the next good frame.
        assert_eq!(check(&mut r, STYLE, "none", 100, S * 7), None);
        assert_eq!(r.previous_state.as_deref(), Some("none"));
    }

    #[test]
    fn classic_reaction_speed_mapping() {
        assert_eq!(classic_game_pair(70.0), Pair { speed: 150, accel: 105 });
        assert_eq!(classic_game_pair(85.0), Pair { speed: 202, accel: 53 });
        assert_eq!(classic_game_pair(120.0), Pair { speed: 255, accel: 0 });
    }

    #[test]
    fn sv_calc_reaction_speed() {
        // The SV target pair.
        assert_eq!(sv_calc_pair("test", true, 85), Pair { speed: 5, accel: 5 });
        // SV: 256 minus rs rescaled from 100..70 to 125..240 (truncated) for both.
        assert_eq!(sv_calc_pair("ac", true, 100), Pair { speed: 131, accel: 131 });
        assert_eq!(sv_calc_pair("ac", true, 70), Pair { speed: 16, accel: 16 });
        // Others: speed is rs rescaled from 70..100 to 150..255, accel 255 minus rs rescaled from 70..100 to 100..255.
        assert_eq!(sv_calc_pair("ac", false, 70), Pair { speed: 150, accel: 155 });
    }

    #[test]
    fn sv_state_change_resets_the_sent_pair_and_holds_the_slow_target() {
        const STYLE: Style = Style::Sv { sv_model: true };
        let mut r = ReactionSpeed::new();
        // Change (None → 'ac'): sent := 6/6, trigger; within 3 s target := 5/5 → differs → send 5/5.
        assert_eq!(check(&mut r, STYLE, "ac", 100, Duration::ZERO), Some(Pair { speed: 5, accel: 5 }));
        assert_eq!(check(&mut r, STYLE, "ac", 100, S * 2), None);
        // > 3 s: trigger cleared, target stays until the next frame follows the SV target pair.
        assert_eq!(check(&mut r, STYLE, "ac", 100, S * 4), None);
        assert_eq!(check(&mut r, STYLE, "ac", 100, S * 5), Some(Pair { speed: 131, accel: 131 }));
    }

    /// Runs `init_reaction_speed` on a manual clock; returns each pair with the time it was sent.
    fn startup(style: Style, state: &str, reaction_speed: i64) -> Vec<(Duration, Pair)> {
        let clock = crate::motion::testing::FakeClock::default();
        let mut sleeper = clock.clone();
        let mut sent = Vec::new();
        init_reaction_speed(
            style,
            &mut sleeper,
            || (state.to_string(), reaction_speed),
            |pair| sent.push((clock.now(), pair)),
        );
        sent
    }

    #[test]
    fn startup_schedule() {
        // init_reaction_speed: send (4, 200), sleep 2 s, five times; then the game pair if a game is running.
        let sent = startup(Style::Classic, "ac", 85);
        let slow = Pair { speed: 4, accel: 200 };
        let expected: Vec<(Duration, Pair)> = (0..5).map(|i| (S * 2 * i, slow)).collect();
        assert_eq!(&sent[..5], &expected[..]);
        // The start-up acceleration rescales to 100..255: accel is 255 minus rs rescaled from 70..100 to 100..255.
        assert_eq!(sent[5], (S * 10, Pair { speed: 202, accel: 78 }));
        assert_eq!(sent.len(), 6);
    }

    #[test]
    fn startup_idle_has_no_final_send() {
        let sent = startup(Style::Sv { sv_model: true }, "none", 100);
        assert_eq!(sent.iter().map(|(_, p)| *p).collect::<Vec<_>>(), vec![Pair { speed: 5, accel: 5 }; 5]);
    }
}
