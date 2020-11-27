use super::ssh;
use serde::{Deserialize, Serialize};
use uuid;
use log;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error(transparent)]
    SshError(#[from] ssh::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub enum Action {
    #[serde(rename = "Shutdown RPi")]
    RpiShutdown,
    #[serde(rename = "Reboot RPi")]
    RpiReboot,
    #[serde(rename = "Identify")]
    Identify,
}

#[derive(Debug)]
pub struct PiPuck {
    pub uuid: uuid::Uuid,
    pub ssh: ssh::Device,
}

impl PiPuck {
    
    pub fn new(ssh: ssh::Device) -> Self {
        Self {
            uuid: uuid::Uuid::new_v4(), 
            ssh,
        }
    }

    pub fn actions(&self) -> Vec<Action> {
        vec![Action::RpiShutdown, Action::RpiReboot, Action::Identify]
    }

    pub async fn execute(&mut self, action: &Action) {
        /* check to see if the requested action is still valid */
        if self.actions().contains(&action) {
            match action {
                Action::RpiShutdown => {
                    if let Err(error) = self.ssh.exec("shutdown 0; exit", false).await {
                        log::error!("{:?} failed with: {}", action, error);
                    }
                },
                Action::RpiReboot => {
                    if let Err(error) = self.ssh.exec("reboot; exit", false).await {
                        log::error!("{:?} failed with: {}", action, error);
                    }
                },
                Action::Identify => {
                    log::error!("pipuck::Action::Identify is not implemented")
                }
            }
        }
        else {
            log::warn!("{:?} ignored due to change in Pi-Puck state", action);
        }
    }
}