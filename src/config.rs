use std::collections::HashMap;

// use rust_decimal::Decimal;

use tokio::{fs::File, io::AsyncReadExt};

use serde::Deserialize;

use crate::entities::Plext;

#[derive(Deserialize)]
pub struct Config {
    pub zones: Vec<Zone>,
}

#[derive(Deserialize)]
pub struct Zone {
    pub from: [u64; 2],
    pub to: [u64; 2],
    pub users: HashMap<u64, Filter>,
}

#[derive(Deserialize)]
pub struct Filter {
    #[serde(rename = "sendAll")]
    pub send_all: bool,
    // pub portals: Option<Vec<(Decimal, Decimal)>>,
    pub portals: Option<Vec<String>>,
    #[serde(rename = "minMU")]
    pub min_mu: Option<usize>,
    pub agents: Option<Vec<String>>,
    pub text: Option<String>,
}

impl Filter {
    pub fn apply(&self, msg: &Plext<'_>) -> bool {
        // if let Some(portals) = &self.portals {
        //     match msg {
        //         Plext::Captured { portal, .. } => {
        //             if portals.contains(&portal.get_coords()) {
        //                 return true;
        //             }
        //         }
        //         Plext::CreatedCF { portal, .. } => {
        //             if portals.contains(&portal.get_coords()) {
        //                 return true;
        //             }
        //         }
        //         Plext::DestroyedCF { portal, .. } => {
        //             if portals.contains(&portal.get_coords()) {
        //                 return true;
        //             }
        //         }
        //         Plext::DeployedReso { portal, .. } => {
        //             if portals.contains(&portal.get_coords()) {
        //                 return true;
        //             }
        //         }
        //         Plext::DestroyedReso { portal, .. } => {
        //             if portals.contains(&portal.get_coords()) {
        //                 return true;
        //             }
        //         }
        //         Plext::DestroyedLink { source, target, .. } => {
        //             if portals.contains(&source.get_coords()) {
        //                 return true;
        //             }
        //             if portals.contains(&target.get_coords()) {
        //                 return true;
        //             }
        //         }
        //         Plext::Linked { source, target, .. } => {
        //             if portals.contains(&source.get_coords()) {
        //                 return true;
        //             }
        //             if portals.contains(&target.get_coords()) {
        //                 return true;
        //             }
        //         }
        //         Plext::DeployedBeacon { portal, .. } => {
        //             if portals.contains(&portal.get_coords()) {
        //                 return true;
        //             }
        //         }
        //         Plext::DeployedFireworks { portal, .. } => {
        //             if portals.contains(&portal.get_coords()) {
        //                 return true;
        //             }
        //         }
        //         _ => {}
        //     }
        // }
        if let Some(min_mu) = &self.min_mu {
            match msg {
                Plext::CreatedCF { mu, .. } => {
                    if mu > min_mu {
                        return true;
                    }
                }
                Plext::DestroyedCF { mu, .. } => {
                    if mu > min_mu {
                        return true;
                    }
                }
                _ => {}
            }
        }
        if let Some(agents) = &self.agents {
            match msg {
                Plext::Captured { player, .. } => {
                    if agents.iter().any(|s| s == player.get_name()) {
                        return true;
                    }
                }
                Plext::CreatedCF { player, .. } => {
                    if agents.iter().any(|s| s == player.get_name()) {
                        return true;
                    }
                }
                Plext::DestroyedCF { player, .. } => {
                    if agents.iter().any(|s| s == player.get_name()) {
                        return true;
                    }
                }
                Plext::DeployedReso { player, .. } => {
                    if agents.iter().any(|s| s == player.get_name()) {
                        return true;
                    }
                }
                Plext::DestroyedReso { player, .. } => {
                    if agents.iter().any(|s| s == player.get_name()) {
                        return true;
                    }
                }
                Plext::DestroyedLink { player, .. } => {
                    if agents.iter().any(|s| s == player.get_name()) {
                        return true;
                    }
                }
                Plext::Linked { player, .. } => {
                    if agents.iter().any(|s| s == player.get_name()) {
                        return true;
                    }
                }
                Plext::DroneReturn { player, .. } => {
                    if agents.iter().any(|s| s == player.get_name()) {
                        return true;
                    }
                }
                Plext::DeployedBeacon { player, .. } => {
                    if agents.iter().any(|s| s == player.get_name()) {
                        return true;
                    }
                }
                Plext::DeployedFireworks { player, .. } => {
                    if agents.iter().any(|s| s == player.get_name()) {
                        return true;
                    }
                }
                _ => {}
            }
        }
        if let Some(s) = &self.text {
            if let Plext::Unknown { text, .. } = msg {
                if text.contains(s) {
                    return true;
                }
            }
        }
        self.send_all
    }
}

/// reads config file
pub async fn get() -> Result<Config, ()> {
    let mut file = File::open("config.yaml")
        .await
        .map_err(|e| eprintln!("Error opening file config.yaml: {}", e))?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .await
        .map_err(|e| eprintln!("Error reading file config.yaml: {}", e))?;
    serde_yaml::from_str(&contents).map_err(|e| eprintln!("Error decoding file config.yaml: {}", e))
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn read() {
        tracing_subscriber::fmt::try_init().ok();
        super::get().await.unwrap();
    }
}
