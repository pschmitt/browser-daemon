//! Which profiles each peer had when it last answered, kept across restarts.
//!
//! While a peer is down, this tells a new profile from one that peer may hold, so a profile can
//! be created on first use without risking an empty copy of one on a restarting node.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::mesh::MeshOverview;

type Inventory = HashMap<String, BTreeSet<String>>;

pub(crate) struct PeerMemory {
    path: PathBuf,
    inventory: Mutex<Inventory>,
}

impl PeerMemory {
    /// A missing or broken file only means nothing is remembered yet.
    pub(crate) fn load(path: &Path) -> Self {
        let inventory = std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            path: path.to_path_buf(),
            inventory: Mutex::new(inventory),
        }
    }

    /// Remember the peers that answered with their whole profile list.
    pub(crate) async fn learn(&self, overview: &MeshOverview) {
        let snapshot = {
            let mut inventory = self.inventory.lock().unwrap_or_else(|e| e.into_inner());
            let mut changed = false;
            for peer in overview.peers.iter().filter(|p| p.status == "online") {
                let names: BTreeSet<String> = overview
                    .profiles
                    .iter()
                    .filter(|p| p.node_name == peer.name)
                    .map(|p| p.profile.name.clone())
                    .collect();
                if names.len() >= peer.profiles_count && inventory.get(&peer.name) != Some(&names) {
                    inventory.insert(peer.name.clone(), names);
                    changed = true;
                }
            }
            changed.then(|| inventory.clone())
        };
        let Some(snapshot) = snapshot else { return };
        if let Err(err) = self.save(&snapshot).await {
            tracing::warn!(error = %err, "Could not save the peers' profile lists");
        }
    }

    async fn save(&self, inventory: &Inventory) -> std::io::Result<()> {
        let bytes = serde_json::to_vec(inventory)?;
        let temporary = self.path.with_extension("json.tmp");
        tokio::fs::write(&temporary, bytes).await?;
        tokio::fs::rename(&temporary, &self.path).await
    }

    /// The peers that are down and had this profile when they last answered.
    pub(crate) fn down_peers_holding(&self, overview: &MeshOverview, name: &str) -> Vec<String> {
        let inventory = self.inventory.lock().unwrap_or_else(|e| e.into_inner());
        overview
            .peers
            .iter()
            .filter(|p| p.status != "online")
            .filter(|p| inventory.get(&p.name).is_some_and(|n| n.contains(name)))
            .map(|p| p.name.clone())
            .collect()
    }
}
