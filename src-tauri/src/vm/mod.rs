pub mod clone;
pub mod config;
pub mod hyperv;
pub mod lifecycle;
pub mod network;
pub mod sandbox;

use crate::target_id::TargetProfile;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;

/// represents the lifecycle state of the ephemeral sandbox
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum VmState {
    Offline,
    Provisioning, // cloning differencing disk
    Booting,      // hyper-v start-vm
    Running,      // os is up, waiting for rdp
    Hooked,       // kernel patches applied, loopback active
    Nuked,        // scorched earth, traces wiped
    Error(String),
}

/// the central controller for spinning up and tearing down the localized matrix.
pub struct VmOrchestrator {
    pub profile: TargetProfile,
    pub vm_name: String,
    pub state: Arc<Mutex<VmState>>,
    pub loopback_ip: String,
}

impl VmOrchestrator {
    /// initializes a new orchestrator for a specific proctoring target
    pub fn new(profile: TargetProfile) -> Self {
        // generate a pseudo-random name to avoid hyper-v collisions 
        // and avoid static string detection by host-level scans
        let entropy = uuid::Uuid::new_v4().to_string();
        let vm_name = format!("win_sys_{}", &entropy[..8]);

        Self {
            profile,
            vm_name,
            state: Arc::new(Mutex::new(VmState::Offline)),
            loopback_ip: "127.0.0.2".to_string(), // hardcoded internal loopback route
        }
    }

    /// the main ignition sequence. executes the full deployment pipeline.
    pub async fn engage(&self) -> Result<(), String> {
        let mut state = self.state.lock().await;
        *state = VmState::Provisioning;
        
        // 1. generate config based on target requirements
        let _cfg = config::build_config(&self.profile);
        
        // 2. clone base vhdx to a fast differencing disk
        clone::spawn_differencing_disk(&self.vm_name).map_err(|e| e.to_string())?;
        
        // 3. setup isolated virtual switch mapped to loopback
        network::configure_loopback_vswitch(&self.vm_name, &self.loopback_ip).map_err(|e| e.to_string())?;

        *state = VmState::Booting;
        
        // 4. register and boot the vm via hyper-v wmi
        hyperv::start_vm(&self.vm_name).map_err(|e| e.to_string())?;
        
        // 5. wait for rdp port 3389 to become responsive on 127.0.0.2
        lifecycle::wait_for_rdp(&self.loopback_ip).await.map_err(|e| e.to_string())?;

        *state = VmState::Running;

        Ok(())
    }

    /// injects the target-specific kernel and registry patches via powershell remoting
    pub async fn apply_stealth_hooks(&self) -> Result<(), String> {
        let mut state = self.state.lock().await;
        
        // this would call into the `patches/` module we'll build later
        // crate::patches::loader::inject_payload(&self.loopback_ip, &self.profile.patch_set_id)?;

        *state = VmState::Hooked;
        Ok(())
    }

    /// completely obliterates the vm, differencing disk, and vswitch. leaves no host artifacts.
    pub async fn teardown(&self) -> Result<(), String> {
        let mut state = self.state.lock().await;
        
        lifecycle::destroy_vm(&self.vm_name).map_err(|e| e.to_string())?;
        network::teardown_vswitch(&self.vm_name).map_err(|e| e.to_string())?;
        
        *state = VmState::Nuked;
        Ok(())
    }

    /// returns the current state for the frontend telemetry feed
    pub async fn get_status(&self) -> VmState {
        self.state.lock().await.clone()
    }
}