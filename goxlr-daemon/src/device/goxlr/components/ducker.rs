use crate::device::goxlr::components::routing_handler::RoutingHandler;
use crate::device::goxlr::device::GoXLR;
use anyhow::{bail, Result};
use async_trait::async_trait;
use goxlr_shared::channels::ducking::DuckingInput;
use goxlr_usb::events::commands::CommandSender;
use log::debug;
use std::collections::HashSet;
use tokio::sync::oneshot;
use goxlr_shared::gate::GateTimes;
use goxlr_shared::mute::MuteState;

const MIC_DB_MAX: f64 = -72.2;

#[derive(Default)]
pub(crate) struct AudioDucker {
    temp: TempDucking,
    ducking_calc: DuckingCalculator,
    noise_gate: SimulatedNoiseGate,
}

#[derive(Default)]
struct TempDucking {
    ducking_index: usize,
    unducking_index: usize,

    last_duck_time: u64,
    last_unduck_time: u64,
}

#[async_trait]
pub(crate) trait AudioDuckerTrait {
    fn is_ducker_active(&self) -> bool;

    async fn handle_ducking(&mut self);

    async fn grab_mic_db(&self) -> Result<f64>;

    async fn handle_ducking_calculations(&mut self);
    async fn run_ducking(&mut self, volume: u8);
}

#[async_trait]
impl AudioDuckerTrait for GoXLR {
    fn is_ducker_active(&self) -> bool {
        self.profile
            .ducking
            .input_source
            .iter()
            .any(|(_, &state)| state)
    }

    async fn handle_ducking(&mut self) {
        // Pre-check if ducking is enabled.
        if !self.profile.ducking.enabled {
            return;
        }

        let mut should_duck = false;
        for input_source in self.profile.ducking.input_source {
            let (input, state) = input_source;

            if state {
                should_duck = true;
                match input {
                    DuckingInput::Mic => {
                        if let Ok(db) = self.grab_mic_db().await {
                            let (name, ducking_state) = self.handle_mic_calculations(db);
                            self.ducking
                                .ducking_calc
                                .handle_result(&name, ducking_state);
                        }
                    } // In case we would add os level DuckingInputs like Chat, we could make them run
                      // in a different thread, add all the values that are running within those 20ms
                      // and make an average of them to use in here, must be stored thread safe of course.
                }
            }
        }

        // Don't go any further at this point.
        if !should_duck {
            return;
        }

        self.handle_ducking_calculations().await;
    }

    async fn grab_mic_db(&self) -> Result<f64> {
        let (msg_send, msg_receive) = oneshot::channel();
        if let Some(sender) = self.command_sender.clone() {
            let command = CommandSender::GetMicLevel(msg_send);
            let _ = sender.send(command).await;
            if let Ok(value) = msg_receive.await {
                return value;
            }
        }
        bail!("[Ducker] Couldn't retrieve mic db value!")
    }

    //noinspection t
    async fn handle_ducking_calculations(&mut self) {
        if self.profile.ducking.transition.ducking.is_empty()
            || self.profile.ducking.transition.unducking.is_empty()
        {
            debug!("[Ducker] Either Ducking or Unducking transition is empty!");
            return;
        }

        let calc = &self.ducking.ducking_calc;

        if calc.need_duck_time_reset() {
            self.ducking.temp.last_duck_time = 0;
        } else if calc.need_unduck_time_reset() {
            self.ducking.temp.last_unduck_time = 0;
        }

        if calc.need_first_duck() {
            // For the switchover to ducking

            let (allowed, volume) = self.handle_first(true);
            if allowed {
                self.run_ducking(volume).await;
            }
        } else if calc.need_other_duck(
            self.profile
                .ducking
                .transition
                .ducking
                .len()
                .saturating_div(1),
            self.ducking.temp.ducking_index,
        ) {
            // While proceeding ducking
            let (allowed, volume) = self.handle_other(true);
            if allowed {
                self.run_ducking(volume).await;
            }
        } else if calc.need_first_unduck() {
            // For the switchover to unducking

            let (allowed, volume) = self.handle_first(false);
            if allowed {
                self.run_ducking(volume).await;
            }
        } else if calc.need_other_unduck(
            self.profile
                .ducking
                .transition
                .unducking
                .len()
                .saturating_div(1),
            self.ducking.temp.unducking_index,
        ) {
            // While proceeding unducking

            let (allowed, volume) = self.handle_other(false);
            if allowed {
                self.run_ducking(volume).await;
            }
        }
    }

    //noinspection t
    async fn run_ducking(&mut self, volume: u8) {
        for (input, input_map) in self.profile.ducking.output_routing {
            for (output, state) in input_map {
                let mut changed = false;
                if state {
                    match self.set_route_value(input, output.into(), volume) {
                        Ok(_) => {
                            changed = true;
                        }
                        Err(err) => {
                            debug!("[Ducker] Error setting route value: {}", err);
                        }
                    }
                }

                if changed {
                    if let Err(err) = self.apply_routing_for_channel(input).await {
                        debug!("[Ducker] Error applying route value: {}", err);
                    }
                }
            }
        }
    }
}

trait InternalAudioDucker {
    fn update_check_time(&mut self, duck: bool, time: u64) -> bool;
    fn handle_first(&mut self, duck: bool) -> (bool, u8);
    fn handle_other(&mut self, duck: bool) -> (bool, u8);
    fn handle_mic_calculations(&mut self, db: f64) -> (String, bool);
    fn noise_gate(
        &mut self,
        db_input: f64,
        threshold: i8,
        attenuation: u8,
        attack: u16,
        release: u16,
    ) -> f64;
}

impl InternalAudioDucker for GoXLR {
    fn update_check_time(&mut self, duck: bool, time: u64) -> bool {
        let last_time = if duck {
            self.ducking.temp.last_duck_time
        } else {
            self.ducking.temp.last_unduck_time
        };

        if last_time < time {
            if duck {
                self.ducking.temp.last_duck_time += self.timer_interval;
            } else {
                self.ducking.temp.last_unduck_time += self.timer_interval;
            }

            return false;
        }

        return true;
    }

    fn handle_first(&mut self, duck: bool) -> (bool, u8) {
        // First check if we waited the attack/release time before going further.

        let at_time = if duck {
            self.profile.ducking.attack_time
        } else {
            self.profile.ducking.release_time
        };

        if !self.update_check_time(duck, at_time) {
            return (false, 0);
        }

        self.ducking.ducking_calc.in_duck_mode = duck;
        self.ducking.ducking_calc.in_ducking = duck;
        self.ducking.ducking_calc.in_unducking = !duck;

        let route_volume = if duck {
            self.ducking.temp.ducking_index += 1;
            self.ducking.temp.last_unduck_time = 0;
            self.ducking.temp.unducking_index = 0;
            self.profile.ducking.transition.ducking[0].route_volume
        } else {
            self.ducking.temp.unducking_index += 1;
            self.ducking.temp.last_duck_time = 0;
            self.ducking.temp.ducking_index = 0;
            self.profile.ducking.transition.unducking[0].route_volume
        };

        (true, route_volume)
    }

    fn handle_other(&mut self, duck: bool) -> (bool, u8) {
        // Check if we waited enough in between the lowering.

        let wait_time = if duck {
            self.profile.ducking.transition.ducking[self.ducking.temp.ducking_index - 1].wait_time
        } else {
            self.profile.ducking.transition.unducking[self.ducking.temp.unducking_index - 1]
                .wait_time
        };

        if !self.update_check_time(duck, wait_time) {
            return (false, 0);
        }

        let route_volume = if duck {
            let index = self.ducking.temp.ducking_index;
            self.ducking.temp.ducking_index += 1;
            self.ducking.temp.last_duck_time = 0;
            self.ducking.temp.unducking_index = 0;
            self.profile.ducking.transition.ducking[index].route_volume
        } else {
            let index = self.ducking.temp.unducking_index;
            self.ducking.temp.unducking_index += 1;
            self.ducking.temp.last_unduck_time = 0;
            self.ducking.temp.ducking_index = 0;
            self.profile.ducking.transition.unducking[index].route_volume
        };

        (true, route_volume)
    }

    fn handle_mic_calculations(&mut self, db: f64) -> (String, bool) {
        // TODO Noise Gate calculations!

        let new_db = self.noise_gate(
            db,
            self.mic_profile.gate.threshold + 12,
            self.mic_profile.gate.attenuation,
            self.mic_profile.gate.attack.to_u16(),
            self.mic_profile.gate.release.to_u16(),
        );

        // Threshold, Attenuation, Attack, Release

        //debug!("{}", &db);

        if new_db >= self.mic_profile.gate.threshold as f64 {
            (DuckingInput::Mic.to_string(), true)
        } else {
            (DuckingInput::Mic.to_string(), false)
        }
    }

    fn noise_gate(
        &mut self,
        db_input: f64,
        threshold_db: i8,
        attenuation_pct: u8,
        attack_ms: u16,
        release_ms: u16,
    ) -> f64 {
        if self.profile.cough.mute_state != MuteState::Unmuted {
            return MIC_DB_MAX;
        }

        // https://en.wikipedia.org/wiki/Noise_gate
        let mut output_db = db_input;

        if output_db < threshold_db as f64 {
            // Signal is below the threshold
            self.ducking.noise_gate.last_release += self.timer_interval;
            self.ducking.noise_gate.last_attack = 0;

            output_db = self.ducking.noise_gate.last_attack_db;

            if self.ducking.noise_gate.last_release > release_ms as u64 {
                self.ducking.noise_gate.last_release = release_ms as u64;

                if !self.ducking.noise_gate.was_above {
                    self.ducking.noise_gate.was_above = true;
                    output_db = MIC_DB_MAX;
                }
            }

            //output_db = output_db - ((MIC_DB_MAX - self.ducking.noise_gate.last_attack_db) / release_ms as f64) * self.ducking.noise_gate.last_release as f64;
        } else {
            // Signal is above the threshold
            self.ducking.noise_gate.last_attack += self.timer_interval;
            self.ducking.noise_gate.last_release = 0;
            self.ducking.noise_gate.was_above = false;

            if self.ducking.noise_gate.last_attack > attack_ms as u64 {
                self.ducking.noise_gate.last_attack = attack_ms as u64;
            }

            output_db = MIC_DB_MAX - ((MIC_DB_MAX - output_db) / attack_ms as f64) * self.ducking.noise_gate.last_attack as f64;
            //output_db = 0.;
        }

        if output_db < threshold_db as f64 {
            output_db = MIC_DB_MAX
        }

        self.ducking.noise_gate.last_attack_db = output_db;

        output_db
    }
}

struct SimulatedNoiseGate {
    last_attack: u64,
    last_release: u64,
    last_attack_db: f64,
    was_above: bool,
}

impl Default for SimulatedNoiseGate {
    fn default() -> Self {
        Self {
            last_attack: Default::default(),
            last_release: GateTimes::Time2000ms.to_u16() as u64,
            last_attack_db: Default::default(),
            was_above: Default::default()
        }
    }
}

#[derive(Clone, Default)]
struct DuckingCalculator {
    in_duck_mode: bool,
    in_ducking: bool,
    in_unducking: bool,

    set: HashSet<String>,
    is_empty: bool,
}

impl DuckingCalculator {
    fn handle_result(&mut self, name: &String, state: bool) {
        if state {
            self.set.insert(name.clone());
        } else {
            self.set.remove(name);
        }

        self.is_empty = self.set.is_empty();
    }

    fn need_duck_time_reset(&self) -> bool {
        self.is_empty && !self.in_duck_mode && !self.in_ducking
    }

    fn need_first_duck(&self) -> bool {
        !self.is_empty && !self.in_duck_mode && !self.in_ducking
    }

    fn need_other_duck(&self, size: usize, index: usize) -> bool {
        self.in_duck_mode && self.in_ducking && !self.in_unducking && size > 0 && index < size
    }

    fn need_unduck_time_reset(&self) -> bool {
        !self.is_empty && self.in_duck_mode && self.in_ducking
    }

    fn need_first_unduck(&self) -> bool {
        self.is_empty && self.in_duck_mode && !self.in_unducking
    }

    fn need_other_unduck(&self, size: usize, index: usize) -> bool {
        !self.in_duck_mode && !self.in_ducking && self.in_unducking && size > 0 && index < size
    }
}
