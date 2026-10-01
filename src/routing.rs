use anyhow::{bail, Context, Result};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::identity::ConfiguredSource;
use crate::pipewire_sink::VirtualSink;
use crate::pulse::{PulseCtl, SinkInput};
use crate::shell::CommandRunner;

const PW_LINK_COMMAND_TIMEOUT: Duration = Duration::from_secs(3);
const PW_LINK_PORT_APPEAR_TIMEOUT: Duration = Duration::from_secs(2);
/// Volume added per step when ramping back to normal after ducking.
const RAMP_UP_STEP_PERCENT: u8 = 4;
/// Shortest pause between two ramp steps.
const RAMP_UP_MIN_STEP_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Debug, Clone)]
pub struct RoutingOptions {
    pub virtual_sink_prefix: String,
    pub sink_appear_timeout: Duration,
}

impl Default for RoutingOptions {
    fn default() -> Self {
        Self {
            virtual_sink_prefix: "pw-duck".to_string(),
            sink_appear_timeout: Duration::from_secs(2),
        }
    }
}

pub struct RoutingEngine<'a, R: CommandRunner> {
    runner: &'a R,
    voice_source: ConfiguredSource,
}

impl<'a, R: CommandRunner> RoutingEngine<'a, R> {
    pub fn new(runner: &'a R, voice_source: ConfiguredSource) -> Self {
        Self {
            runner,
            voice_source,
        }
    }

    pub fn start(&mut self, options: RoutingOptions) -> Result<RouteSession<'a, R>> {
        if !self.voice_source.has_match_fields() {
            anyhow::bail!("configured voice source contains no stable match fields");
        }

        let pulse = PulseCtl::new(self.runner);
        let original_default_sink_name = pulse.default_sink_name()?;
        let real_sink = self.real_sink_for_session(&pulse)?;
        let real_sink_name = real_sink.name.clone();

        let virtual_sink = VirtualSink::create(&options.virtual_sink_prefix)?;
        let virtual_sink_name = virtual_sink.name().to_string();
        let virtual_pulse_sink =
            pulse.wait_for_sink(&virtual_sink_name, options.sink_appear_timeout)?;
        pulse
            .set_default_sink(&original_default_sink_name)
            .with_context(|| format!("restore default sink after creating {virtual_sink_name}"))?;

        let links = PipeWireLinks::new(self.runner);
        let monitor_links =
            links.link_stereo_monitor_to_sink(&virtual_sink_name, &real_sink_name)?;

        let mut session = RouteSession {
            runner: self.runner,
            virtual_sink,
            virtual_sink_name,
            virtual_sink_index: virtual_pulse_sink.index,
            original_default_sink_name,
            real_sink_name,
            real_sink_index: real_sink.index,
            moved_inputs: BTreeMap::new(),
            monitor_links,
            stopped: false,
            applied_percent: Cell::new(100),
            release_fade: RefCell::new(None),
        };

        session.route_existing_outputs(&self.voice_source)?;
        session.set_neutral()?;
        Ok(session)
    }

    fn real_sink_for_session(&self, pulse: &PulseCtl<'a, R>) -> Result<crate::pulse::Sink> {
        if let Some(voice_input) = pulse.sink_inputs()?.into_iter().find(|input| {
            input
                .identity()
                .matches_configured_source(&self.voice_source)
        }) {
            return pulse
                .sink_by_index(voice_input.sink)?
                .with_context(|| format!("voice source sink #{} not found", voice_input.sink));
        }

        let default_sink_name = pulse.default_sink_name()?;
        pulse
            .sink_by_name(&default_sink_name)?
            .with_context(|| format!("default sink {default_sink_name} not found"))
    }
}

pub struct RouteSession<'a, R: CommandRunner> {
    runner: &'a R,
    virtual_sink: VirtualSink,
    virtual_sink_name: String,
    virtual_sink_index: u32,
    original_default_sink_name: String,
    real_sink_name: String,
    real_sink_index: u32,
    moved_inputs: BTreeMap<u32, u32>,
    monitor_links: Vec<PortLink>,
    stopped: bool,
    applied_percent: Cell<u8>,
    release_fade: RefCell<Option<ReleaseFade>>,
}

impl<'a, R: CommandRunner> RouteSession<'a, R> {
    pub fn set_duck_percent(&self, percent: u8) -> Result<()> {
        let percent = percent.min(100);
        self.cancel_release_fade();
        PulseCtl::new(self.runner)
            .set_source_volume_percent(&self.virtual_monitor_name(), percent)?;
        self.applied_percent.set(percent);
        Ok(())
    }

    pub fn set_neutral(&self) -> Result<()> {
        self.cancel_release_fade();
        PulseCtl::new(self.runner).set_source_volume_percent(&self.virtual_monitor_name(), 100)?;
        self.applied_percent.set(100);
        Ok(())
    }

    /// Come back to full volume over `fade` instead of jumping, so the return to
    /// normal level is not audible as a step. A zero fade restores at once.
    ///
    /// This blocks the caller, so only use it while the session is ending. While
    /// ducking is driven by a live detector use [`Self::begin_release_fade`] plus
    /// [`Self::advance_release_fade`], which can be interrupted by new ducking.
    pub fn ramp_to_neutral(&self, fade: Duration) -> Result<()> {
        self.cancel_release_fade();
        let mut percent = self.applied_percent.get();
        if percent >= 100 || fade.is_zero() {
            return self.set_neutral();
        }

        let interval = ramp_step_interval(percent, fade);
        while percent < 100 {
            percent = next_ramp_step(percent);
            if let Err(err) = PulseCtl::new(self.runner)
                .set_source_volume_percent(&self.virtual_monitor_name(), percent)
            {
                eprintln!(
                    "volume ramp stopped at {percent}%: {err:#}; restoring full volume directly"
                );
                self.applied_percent.set(100);
                return PulseCtl::new(self.runner)
                    .set_source_volume_percent(&self.virtual_monitor_name(), 100);
            }
            self.applied_percent.set(percent);
            if percent < 100 {
                std::thread::sleep(interval);
            }
        }

        Ok(())
    }

    /// Start a fade back to full volume that is advanced by [`Self::advance_release_fade`]
    /// instead of blocking. Any later volume change cancels it, so new voice activity
    /// can duck again while the music is still coming back up.
    pub fn begin_release_fade(&self, fade: Duration) -> Result<()> {
        self.cancel_release_fade();

        let from = self.applied_percent.get();
        if fade.is_zero() || from >= 100 {
            return self.set_neutral();
        }

        let steps = ramp_step_count(from) as u32;
        self.release_fade.borrow_mut().replace(ReleaseFade {
            from,
            steps,
            taken: 0,
            interval: ramp_step_interval(from, fade),
            next_step_at: Instant::now(),
        });
        Ok(())
    }

    /// Apply at most one fade step if its time has come. Cheap to call every loop.
    pub fn advance_release_fade(&self) {
        let step = {
            let mut slot = self.release_fade.borrow_mut();
            let Some(fade) = slot.as_mut() else {
                return;
            };
            let step = take_fade_step(fade, Instant::now());
            if fade.taken >= fade.steps {
                slot.take();
            }
            step
        };

        let Some(percent) = step else {
            return;
        };

        if let Err(err) = PulseCtl::new(self.runner)
            .set_source_volume_percent(&self.virtual_monitor_name(), percent)
        {
            eprintln!(
                "release fade stopped at {percent}%: {err:#}; restoring full volume directly"
            );
            self.cancel_release_fade();
            let _ = self.set_neutral();
            return;
        }

        self.applied_percent.set(percent);
        if percent >= 100 {
            self.cancel_release_fade();
        }
    }

    fn cancel_release_fade(&self) {
        self.release_fade.borrow_mut().take();
    }

    fn virtual_monitor_name(&self) -> String {
        format!("{}.monitor", self.virtual_sink_name)
    }

    pub fn route_existing_outputs(&mut self, voice_source: &ConfiguredSource) -> Result<usize> {
        let pulse = PulseCtl::new(self.runner);
        let inputs = pulse.sink_inputs()?;
        let mut moved = 0;

        for input in inputs {
            if is_routed_target(&input, voice_source, self.virtual_sink_index) {
                self.moved_inputs
                    .entry(input.index)
                    .or_insert(self.real_sink_index);
                continue;
            }

            if !should_route(&input, voice_source, self.real_sink_index) {
                continue;
            }

            self.moved_inputs.entry(input.index).or_insert(input.sink);
            pulse.move_sink_input(input.index, &self.virtual_sink_name)?;
            moved += 1;
        }

        Ok(moved)
    }

    pub fn stop(&mut self) -> Result<()> {
        if self.stopped {
            return Ok(());
        }

        let pulse = PulseCtl::new(self.runner);
        let _ = self.set_neutral();
        let _ = pulse.set_default_sink(&self.original_default_sink_name);

        self.record_inputs_still_on_virtual_sink(&pulse)?;

        for (input_index, original_sink_index) in self.moved_inputs.clone() {
            if let Err(err) = pulse.move_sink_input_to_sink_index(input_index, original_sink_index)
            {
                eprintln!(
                    "restore sink-input #{input_index} to sink #{original_sink_index} failed: {err:#}; trying {}",
                    self.real_sink_name
                );
                let _ = pulse.move_sink_input(input_index, &self.real_sink_name);
            }
        }

        if let Err(err) = self.wait_until_virtual_sink_is_empty(&pulse, Duration::from_secs(3)) {
            eprintln!(
                "teardown warning: {err:#}; leaving virtual sink {} alive to avoid terminating audio streams",
                self.virtual_sink_name
            );
            self.virtual_sink.abandon();
            let _ = pulse.set_default_sink(&self.original_default_sink_name);
            self.stopped = true;
            return Ok(());
        }

        let links = PipeWireLinks::new(self.runner);
        for link in self.monitor_links.drain(..) {
            let _ = links.unlink(&link);
        }

        self.virtual_sink.destroy()?;
        let _ = pulse.set_default_sink(&self.original_default_sink_name);
        self.moved_inputs.clear();
        self.stopped = true;
        Ok(())
    }

    fn record_inputs_still_on_virtual_sink(&mut self, pulse: &PulseCtl<'a, R>) -> Result<()> {
        for input in pulse.sink_inputs_on_sink(self.virtual_sink_index)? {
            self.moved_inputs
                .entry(input.index)
                .or_insert(self.real_sink_index);
        }
        Ok(())
    }

    fn wait_until_virtual_sink_is_empty(
        &mut self,
        pulse: &PulseCtl<'a, R>,
        timeout: Duration,
    ) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            let inputs = pulse.sink_inputs_on_sink(self.virtual_sink_index)?;
            if inputs.is_empty() {
                return Ok(());
            }

            for input in inputs {
                self.moved_inputs
                    .entry(input.index)
                    .or_insert(self.real_sink_index);
                let _ = pulse.move_sink_input(input.index, &self.real_sink_name);
            }

            if Instant::now() >= deadline {
                let remaining = pulse
                    .sink_inputs_on_sink(self.virtual_sink_index)?
                    .into_iter()
                    .map(|input| format!("#{}", input.index))
                    .collect::<Vec<_>>()
                    .join(", ");
                anyhow::bail!(
                    "refusing to destroy virtual sink {}; still has sink-input(s): {remaining}",
                    self.virtual_sink_name
                );
            }

            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl<R: CommandRunner> Drop for RouteSession<'_, R> {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Number of volume steps needed to climb from `from_percent` to 100%.
fn ramp_step_count(from_percent: u8) -> u128 {
    let missing = 100u16.saturating_sub(u16::from(from_percent.min(100)));
    u128::from(missing.div_ceil(u16::from(RAMP_UP_STEP_PERCENT)).max(1))
}

/// Volume after `taken` of `steps` fade steps from `from` up to 100%.
fn fade_step_percent(from: u8, steps: u32, taken: u32) -> u8 {
    let steps = steps.max(1);
    let taken = taken.clamp(1, steps);
    let missing = u32::from(100u8.saturating_sub(from));
    let step_size = missing.div_ceil(steps);
    (u32::from(from) + step_size * taken).min(100) as u8
}

/// One step of a running fade, or `None` when it is not due yet or finished.
fn take_fade_step(fade: &mut ReleaseFade, now: Instant) -> Option<u8> {
    if now < fade.next_step_at || fade.taken >= fade.steps {
        return None;
    }

    fade.taken += 1;
    let percent = fade_step_percent(fade.from, fade.steps, fade.taken);
    fade.next_step_at = now + fade.interval;
    Some(percent)
}

#[derive(Debug, Clone, Copy)]
struct ReleaseFade {
    from: u8,
    steps: u32,
    taken: u32,
    interval: Duration,
    next_step_at: Instant,
}

fn next_ramp_step(percent: u8) -> u8 {
    percent.saturating_add(RAMP_UP_STEP_PERCENT).min(100)
}

/// Never takes longer than `fade`, but never faster than the minimum step
/// interval either, so a very short fade still stays smooth.
fn ramp_step_interval(from_percent: u8, fade: Duration) -> Duration {
    let min_interval_ms = RAMP_UP_MIN_STEP_INTERVAL.as_millis().max(1);
    let steps = (fade.as_millis() / min_interval_ms).max(1);
    let steps = steps.min(ramp_step_count(from_percent));
    Duration::from_millis(
        u64::try_from((fade.as_millis() / steps).max(min_interval_ms)).unwrap_or(1),
    )
}

fn should_route(input: &SinkInput, voice_source: &ConfiguredSource, real_sink_index: u32) -> bool {
    let identity = input.identity();
    identity.is_playback_stream()
        && input.sink == real_sink_index
        && !identity.matches_configured_source(voice_source)
}

fn is_routed_target(
    input: &SinkInput,
    voice_source: &ConfiguredSource,
    virtual_sink_index: u32,
) -> bool {
    let identity = input.identity();
    identity.is_playback_stream()
        && input.sink == virtual_sink_index
        && !identity.matches_configured_source(voice_source)
}

#[derive(Debug, Clone)]
struct PortLink {
    output: String,
    input: String,
}

struct PipeWireLinks<'a, R: CommandRunner> {
    runner: &'a R,
}

impl<'a, R: CommandRunner> PipeWireLinks<'a, R> {
    fn new(runner: &'a R) -> Self {
        Self { runner }
    }

    fn link_stereo_monitor_to_sink(
        &self,
        virtual_sink: &str,
        real_sink: &str,
    ) -> Result<Vec<PortLink>> {
        let deadline = Instant::now() + PW_LINK_PORT_APPEAR_TIMEOUT;
        let pairs = loop {
            let outputs = self
                .runner
                .output_with_timeout("pw-link", &["-o"], PW_LINK_COMMAND_TIMEOUT)
                .context("list PipeWire output ports")?;
            let inputs = self
                .runner
                .output_with_timeout("pw-link", &["-i"], PW_LINK_COMMAND_TIMEOUT)
                .context("list PipeWire input ports")?;

            match monitor_links_for_ports(&outputs, &inputs, virtual_sink, real_sink) {
                Ok(pairs) => break pairs,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(err) => {
                    bail!(
                        "PipeWire ports for {virtual_sink} -> {real_sink} did not become available within {:?}: {err:#}",
                        PW_LINK_PORT_APPEAR_TIMEOUT
                    );
                }
            }
        };

        let mut created = Vec::new();
        for link in pairs {
            self.runner
                .status_with_timeout(
                    "pw-link",
                    &[&link.output, &link.input],
                    PW_LINK_COMMAND_TIMEOUT,
                )
                .with_context(|| format!("link {} -> {}", link.output, link.input))?;
            created.push(link);
        }
        Ok(created)
    }

    fn unlink(&self, link: &PortLink) -> Result<()> {
        self.runner
            .status_with_timeout(
                "pw-link",
                &["-d", &link.output, &link.input],
                PW_LINK_COMMAND_TIMEOUT,
            )
            .with_context(|| format!("unlink {} -> {}", link.output, link.input))
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct PipeWirePort {
    full_name: String,
    channel: String,
}

fn monitor_links_for_ports(
    outputs: &str,
    inputs: &str,
    virtual_sink: &str,
    real_sink: &str,
) -> Result<Vec<PortLink>> {
    let monitor_ports = ports_for_node(outputs, virtual_sink, "monitor_");
    let playback_ports = ports_for_node(inputs, real_sink, "playback_");

    if monitor_ports.is_empty() {
        bail!("no monitor ports found for virtual sink {virtual_sink}");
    }
    if playback_ports.is_empty() {
        bail!("no playback ports found for sink {real_sink}");
    }

    let monitor_ports = choose_stereo_ports(&monitor_ports);
    let playback_ports = choose_stereo_ports(&playback_ports);
    let link_count = monitor_ports.len().min(playback_ports.len()).min(2);
    if link_count == 0 {
        bail!(
            "could not pair monitor ports for {virtual_sink} with playback ports for {real_sink}"
        );
    }

    Ok((0..link_count)
        .map(|index| PortLink {
            output: monitor_ports[index].full_name.clone(),
            input: playback_ports[index].full_name.clone(),
        })
        .collect())
}

fn ports_for_node(port_list: &str, node_name: &str, port_prefix: &str) -> Vec<PipeWirePort> {
    let node_prefix = format!("{node_name}:");
    port_list
        .lines()
        .filter_map(|line| {
            let full_name = line.trim();
            let port_name = full_name.strip_prefix(&node_prefix)?;
            let channel = port_name.strip_prefix(port_prefix)?;
            Some(PipeWirePort {
                full_name: full_name.to_string(),
                channel: channel.to_string(),
            })
        })
        .collect()
}

fn choose_stereo_ports(ports: &[PipeWirePort]) -> Vec<PipeWirePort> {
    for pair in [["FL", "FR"], ["AUX0", "AUX1"]] {
        if let Some(selected) = ports_for_channel_pair(ports, pair) {
            return selected;
        }
    }

    ports.iter().take(2).cloned().collect()
}

fn ports_for_channel_pair(ports: &[PipeWirePort], pair: [&str; 2]) -> Option<Vec<PipeWirePort>> {
    pair.into_iter()
        .map(|channel| ports.iter().find(|port| port.channel == channel).cloned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairs_classic_stereo_ports() {
        let links = monitor_links_for_ports(
            "pw-duck-1:monitor_FL\npw-duck-1:monitor_FR\n",
            "alsa_output.foo:playback_FL\nalsa_output.foo:playback_FR\n",
            "pw-duck-1",
            "alsa_output.foo",
        )
        .unwrap();

        assert_eq!(links.len(), 2);
        assert_eq!(links[0].output, "pw-duck-1:monitor_FL");
        assert_eq!(links[0].input, "alsa_output.foo:playback_FL");
        assert_eq!(links[1].output, "pw-duck-1:monitor_FR");
        assert_eq!(links[1].input, "alsa_output.foo:playback_FR");
    }

    #[test]
    fn pairs_virtual_stereo_to_pro_audio_aux_ports() {
        let links = monitor_links_for_ports(
            "pw-duck-1:monitor_FL\npw-duck-1:monitor_FR\n",
            "alsa_output.corsair:playback_AUX0\nalsa_output.corsair:playback_AUX1\n",
            "pw-duck-1",
            "alsa_output.corsair",
        )
        .unwrap();

        assert_eq!(links.len(), 2);
        assert_eq!(links[0].output, "pw-duck-1:monitor_FL");
        assert_eq!(links[0].input, "alsa_output.corsair:playback_AUX0");
        assert_eq!(links[1].output, "pw-duck-1:monitor_FR");
        assert_eq!(links[1].input, "alsa_output.corsair:playback_AUX1");
    }

    #[test]
    fn falls_back_to_first_two_playback_ports() {
        let links = monitor_links_for_ports(
            "pw-duck-1:monitor_FL\npw-duck-1:monitor_FR\n",
            "alsa_output.weird:playback_X\nalsa_output.weird:playback_Y\nalsa_output.weird:playback_Z\n",
            "pw-duck-1",
            "alsa_output.weird",
        )
        .unwrap();

        assert_eq!(links.len(), 2);
        assert_eq!(links[0].input, "alsa_output.weird:playback_X");
        assert_eq!(links[1].input, "alsa_output.weird:playback_Y");
    }

    #[test]
    fn ramp_up_steps_climb_monotonically_to_full_volume() {
        let mut percent = 25u8;
        let mut steps = 0;
        while percent < 100 {
            let next = next_ramp_step(percent);
            assert!(next > percent);
            percent = next;
            steps += 1;
        }

        assert_eq!(percent, 100);
        assert_eq!(steps, 19);
    }

    #[test]
    fn ramp_up_step_from_full_volume_stays_full() {
        assert_eq!(next_ramp_step(100), 100);
        assert_eq!(next_ramp_step(99), 100);
    }

    #[test]
    fn fade_steps_climb_to_full_volume_and_then_stop() {
        let mut fade = ReleaseFade {
            from: 25,
            steps: 19,
            taken: 0,
            interval: Duration::from_millis(30),
            next_step_at: Instant::now(),
        };

        let mut percents = Vec::new();
        for _ in 0..25 {
            match take_fade_step(&mut fade, Instant::now()) {
                Some(percent) => percents.push(percent),
                None => break,
            }
            // Simulate the caller sleeping for one interval.
            fade.next_step_at = Instant::now();
        }

        assert!(percents.windows(2).all(|pair| pair[1] > pair[0]));
        assert_eq!(percents.first().copied(), Some(25 + 4));
        assert_eq!(percents.last().copied(), Some(100));
        assert!(take_fade_step(&mut fade, Instant::now()).is_none());
    }

    #[test]
    fn fade_step_is_not_taken_before_its_interval() {
        let now = Instant::now();
        let mut fade = ReleaseFade {
            from: 64,
            steps: 9,
            taken: 0,
            interval: Duration::from_millis(200),
            next_step_at: now + Duration::from_millis(200),
        };

        assert_eq!(take_fade_step(&mut fade, now), None);
        assert_eq!(fade.taken, 0);
        assert!(take_fade_step(&mut fade, now + Duration::from_millis(200)).is_some());
    }

    #[test]
    fn fade_step_percent_reaches_full_volume_exactly() {
        assert_eq!(fade_step_percent(25, 19, 19), 100);
        assert_eq!(fade_step_percent(100, 1, 1), 100);
        assert_eq!(fade_step_percent(96, 1, 1), 100);
        assert!(fade_step_percent(25, 19, 5) > 25);
    }

    #[test]
    fn ramp_step_interval_fits_requested_fade() {
        let fade = Duration::from_millis(600);
        let interval = ramp_step_interval(25, fade);
        let steps = ramp_step_count(25);

        assert!(interval >= RAMP_UP_MIN_STEP_INTERVAL);
        assert!(
            interval.as_millis() * steps
                <= fade.as_millis() + u128::from(RAMP_UP_MIN_STEP_INTERVAL.as_millis())
        );
    }

    #[test]
    fn long_fades_spread_the_needed_steps_over_the_whole_duration() {
        // 25% -> 100% needs 19 steps; 1500ms means about 78ms per step, not 20ms.
        let interval = ramp_step_interval(25, Duration::from_millis(1500));

        assert_eq!(ramp_step_count(25), 19);
        assert_eq!(interval, Duration::from_millis(78));
    }

    #[test]
    fn ramp_step_interval_keeps_minimum_pause_for_tiny_fades() {
        let interval = ramp_step_interval(25, Duration::from_millis(10));

        assert_eq!(interval, RAMP_UP_MIN_STEP_INTERVAL);
    }

    #[test]
    fn ramp_step_interval_of_zero_fade_is_unused_because_it_restores_at_once() {
        // A zero fade short-circuits in ramp_to_neutral; the helper stays valid anyway.
        let interval = ramp_step_interval(25, Duration::ZERO);

        assert_eq!(interval, RAMP_UP_MIN_STEP_INTERVAL);
    }
}
