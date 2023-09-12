use std::collections::HashSet;
use anyhow::{bail, Result};
use async_trait::async_trait;
use enum_map::EnumMap;
use log::debug;
use tokio::sync::{oneshot};
use goxlr_profile::{DuckingSettings, DuckingTransition, DuckingVolume};
use goxlr_shared::channels::{DuckingInput, InputChannels, OutputChannels};
use goxlr_usb::events::commands::CommandSender;
use crate::device::goxlr::components::routing_handler::RoutingHandler;
use crate::device::goxlr::device::GoXLR;

const MIC_DB_THRESHOLD: f64 = -20.; 

#[derive(Default)]
pub(crate) struct AudioDucker {
    enabled: bool,
    input_source: EnumMap<DuckingInput, bool>,

    output_routing: EnumMap<InputChannels, EnumMap<OutputChannels, bool>>,
    
    transition: DuckingTransition,
    timing: DuckingTiming,
    temp: TempDucking,

    ducking_calculator: DuckingCalculator,
}

#[derive(Default)]
struct DuckingTiming {
    attack_time: u64,
    release_time: u64,
}

#[derive(Default)]
struct TempDucking {
    ducking_size: usize,
    ducking_index: usize, // max = ducking_size - 1
    unducking_size: usize,
    unducking_index: usize, // max = unducking_size - 1
    
    last_duck_time: u64,
    last_unduck_time: u64,
}

impl AudioDucker {
    pub(crate) fn load(&mut self, settings: &DuckingSettings) {
        self.enabled = settings.enabled;
        self.input_source = settings.input_source;
        self.output_routing = settings.output_routing;
        
        self.transition = settings.transition.clone();
        self.timing = DuckingTiming {
            attack_time: settings.attack_time,
            release_time: settings.release_time,
        };
        
        // For easier comparing and handling.
        let mut ducking_size = self.transition.ducking.len();
        if ducking_size != 0 {
            ducking_size -= 1;
        }
        
        let mut unducking_size = self.transition.unducking.len();
        if unducking_size != 0 {
            unducking_size -= 1;
        }
        
        self.temp = TempDucking {
            ducking_size,
            ducking_index: 0,
            unducking_size,
            unducking_index: 0,

            last_duck_time: 0,
            last_unduck_time: 0,
        };
    }
    
    fn is_active(&self) -> bool {
        self.input_source.iter().any(|(_, &state)| state)
    }

    fn set_ducking(&mut self, input: DuckingInput, state: bool) {
        self.input_source[input] = state;
    }
}

#[async_trait]
pub(crate) trait AudioDuckerTrait {
    async fn handle_ducking(&mut self);

    async fn grab_mic_db(&self) -> Result<f64>;

    async fn handle_ducking_calculations(&mut self);
    async fn run_ducking(&mut self, volume: u8);
}

#[async_trait]
impl AudioDuckerTrait for GoXLR {
    async fn handle_ducking(&mut self) {
        // Pre-check if ducking is enabled.
        if !self.ducking.enabled {
            return;
        }
        
        for input_source in self.ducking.input_source {
            let (input, state) = input_source;
            
            if state {
                match input {
                    DuckingInput::Mic => {
                        if let Ok(db) = self.grab_mic_db().await {
                            let (name, ducking_state) = handle_mic_calculations(db);
                            self.ducking.ducking_calculator.handle_result(&name, ducking_state);
                        }
                    }
                    // In case we would add os level DuckingInputs like Chat, we could make them run
                    // in a different thread, add all the values that are running within those 20ms
                    // and make an average of them to use in here, must be stored thread safe of course.
                }
            }
        }
        
        self.handle_ducking_calculations().await;
    }
    
    async fn grab_mic_db(&self) -> Result<f64> {
        let (msg_send, msg_receive) = oneshot::channel();
        if let Some(sender) = self.command_sender.clone() {
            let command = CommandSender::GetMicLevel(msg_send);
            let _ = sender.send(command).await;
            if let Ok(value) = msg_receive.await {
                return value
            }
        }
        bail!("[Ducker] Couldn't retrieve mic db value!")
    }
    
    async fn handle_ducking_calculations(&mut self) {
        if self.ducking.transition.ducking.is_empty() || self.ducking.transition.unducking.is_empty() {
            debug!("[Ducker] Either Ducking or Unducking transition is empty!");
            return;
        }
        
        let is_empty = self.ducking.ducking_calculator.is_empty;
        let state = self.ducking.ducking_calculator.state;
        let in_ducking = self.ducking.ducking_calculator.in_ducking;
        let in_unducking = self.ducking.ducking_calculator.in_unducking;
        
        
        if !is_empty && !state && !in_ducking {
            // For the switchover to ducking

            let (allowed, volume) = handle_first(
                self.timer_interval,
                &mut self.ducking.temp.last_duck_time,
                &mut self.ducking.temp.last_unduck_time,
                &mut self.ducking.timing.attack_time,
                &mut self.ducking.ducking_calculator.state,
                &mut self.ducking.ducking_calculator.in_ducking,
                &mut self.ducking.ducking_calculator.in_unducking,
                &mut self.ducking.temp.ducking_index,
                &mut self.ducking.temp.unducking_index,
                &mut self.ducking.transition.ducking,
            );

            if allowed {
                self.run_ducking(volume).await;
            }
            
        } else if state && in_ducking && !in_unducking && self.ducking.temp.ducking_size > 0 && self.ducking.temp.ducking_index <= self.ducking.temp.ducking_size {
            // While proceeding ducking
            
            let (allowed, volume) = handle_other(
                self.timer_interval,
                &mut self.ducking.temp.last_duck_time,
                &mut self.ducking.temp.ducking_index,
                &mut self.ducking.temp.unducking_index,
                &mut self.ducking.transition.ducking,
            );

            if allowed {
                self.run_ducking(volume).await;
            }
        } else if is_empty && state && !in_unducking {
            // For the switchover to unducking
            
            let (allowed, volume) = handle_first(
                self.timer_interval,
                &mut self.ducking.temp.last_unduck_time,
                &mut self.ducking.temp.last_duck_time,
                &mut self.ducking.timing.release_time,
                &mut self.ducking.ducking_calculator.state,
                &mut self.ducking.ducking_calculator.in_unducking,
                &mut self.ducking.ducking_calculator.in_ducking,
                &mut self.ducking.temp.unducking_index,
                &mut self.ducking.temp.ducking_index,
                &mut self.ducking.transition.unducking,
            );
            
            if allowed {
                self.run_ducking(volume).await;
            }
        } else if !state && in_unducking && self.ducking.temp.unducking_size > 0 && self.ducking.temp.unducking_index <= self.ducking.temp.unducking_size {
            // While proceeding unducking
            let (allowed, volume) = handle_other(
                self.timer_interval,
                &mut self.ducking.temp.last_unduck_time,
                &mut self.ducking.temp.unducking_index,
                &mut self.ducking.temp.ducking_index,
                &mut self.ducking.transition.unducking,
            );

            if allowed {
                self.run_ducking(volume).await;
            }
        }
    }

    async fn run_ducking(&mut self, volume: u8) {
        for (input, input_map) in self.ducking.output_routing {
            for (output, state) in input_map {
                let mut changed = false;
                if state {
                    match self.set_route_value(input, output.into(), volume) {
                        Ok(_) => { changed = true; }
                        Err(err) => { debug!("[Ducker] Error setting route value: {}", err); }
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

fn handle_first(interval: u64, last_time: &mut u64, last_time_zero: &mut u64, time: &mut u64, state: &mut bool, in_correct: &mut bool, in_reverse: &mut bool, index: &mut usize, index_zero: &mut usize, transition: &mut Vec<DuckingVolume>) -> (bool, u8) {
    // First check if we waited the attack time before starting ducking.
    if *last_time < *time {
        *last_time += interval;
        return (false, 0);
    }

    *state = !*state ;
    *in_correct = true;
    *in_reverse = false;

    let route_volume = transition[0].route_volume;
    *index += 1;
    *last_time_zero = 0;
    *index_zero = 0;

    (true, route_volume)
}

fn handle_other(interval: u64, last_time: &mut u64, index: &mut usize, index_zero: &mut usize, transition: &mut Vec<DuckingVolume>) -> (bool, u8) {
    // Check if we waited enough in between the lowering.
    let inner_index = index.clone();
    if last_time < &mut transition[inner_index - 1].wait_time {
        *last_time += interval;
        return (false, 0);
    }

    let route_volume = transition[inner_index].route_volume;
    *index += 1;
    *last_time = 0;
    *index_zero = 0;

    (true, route_volume)
}

fn handle_mic_calculations(db: f64) -> (String, bool) {
    // TODO Noise Gate calculations!
    
    debug!("{} | {}", &db, MIC_DB_THRESHOLD);
    
    if db >= MIC_DB_THRESHOLD {
        (DuckingInput::Mic.to_string(), true)
    } else {
        (DuckingInput::Mic.to_string(), false)
    }
}

#[derive(Clone, Default)]
struct DuckingCalculator {
    state: bool,
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
}