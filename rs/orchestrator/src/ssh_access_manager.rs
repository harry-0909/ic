use crate::{metrics::OrchestratorMetrics, registry_helper::RegistryHelper};
use ic_logger::{debug, warn, ReplicaLogger};
use ic_types::RegistryVersion;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

const REGISTRY_CHECK_INTERVAL: Duration = Duration::from_secs(10);

/// Continuously checks the Registry to determine if there has been a change in
/// the readonly and backup public key sets.If so, updates the accesss to the
/// node accordingly.
pub(crate) struct SshAccessManager {
    registry: Arc<RegistryHelper>,
    metrics: Arc<OrchestratorMetrics>,
    logger: ReplicaLogger,
    last_seen_registry_version: RegistryVersion,
    // If false, do not start or terminate the background task
    enabled: Arc<std::sync::atomic::AtomicBool>,
}

impl SshAccessManager {
    pub(crate) fn new(
        registry: Arc<RegistryHelper>,
        metrics: Arc<OrchestratorMetrics>,
        logger: ReplicaLogger,
    ) -> Self {
        let enabled = Arc::new(std::sync::atomic::AtomicBool::new(true));
        Self {
            registry,
            metrics,
            logger,
            last_seen_registry_version: RegistryVersion::new(0),
            enabled,
        }
    }

    pub(crate) fn start(self) -> Arc<std::sync::atomic::AtomicBool> {
        let result = self.enabled.clone();
        tokio::spawn(background_task(self));
        result
    }

    /// Checks for changes in the keysets, and updates the node accordingly.
    pub(crate) async fn check_for_keyset_changes(&mut self) {
        let registry_version = self.registry.get_latest_version();
        if self.last_seen_registry_version == registry_version {
            return;
        }
        debug!(
            self.logger,
            "Checking for the access keys in the registry version: {}", registry_version
        );

        let (new_readonly_keys, new_backup_keys) = match self
            .registry
            .get_own_readonly_and_backup_keysets(registry_version)
        {
            Err(error) => {
                warn!(
                    every_n_seconds => 300,
                    self.logger,
                    "Cannot retrieve the readonly & backup keysets from the registry {}", error
                );
                return;
            }
            Ok(keys) => keys,
        };

        // Update the readonly & backup keys. If it fails, log why.
        if self.update_access_keys(&new_readonly_keys, &new_backup_keys) {
            self.last_seen_registry_version = registry_version;
            self.metrics
                .ssh_access_registry_version
                .set(registry_version.get() as i64);
        }
    }

    fn update_access_keys(&mut self, readonly_keys: &[String], backup_keys: &[String]) -> bool {
        let mut both_keys_are_successfully_updated: bool = true;
        if let Err(e) = self.update_access_to_one_account("readonly", readonly_keys) {
            warn!(
                every_n_seconds => 300,
                self.logger,
                "Could not update the readonly keys due to a script failure: {}", e
            );
            both_keys_are_successfully_updated = false;
        };
        if let Err(e) = self.update_access_to_one_account("backup", backup_keys) {
            warn!(
                every_n_seconds => 300,
                self.logger,
                "Could not update the backup keys due to a script failure: {}", e
            );
            both_keys_are_successfully_updated = false;
        }
        both_keys_are_successfully_updated
    }

    fn update_access_to_one_account(
        &mut self,
        account: &str,
        keys: &[String],
    ) -> Result<(), String> {
        let mut cmd = Command::new("sudo")
            .arg("/opt/ic/bin/provision-ssh-keys.sh")
            .arg(account)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| format!("Failed to spawn child process: {}", e))?;

        let mut stdin = cmd
            .stdin
            .take()
            .ok_or_else(|| "Failed to open stdin".to_string())?;
        let key_list = keys.join("\n");
        stdin
            .write_all(key_list.as_bytes())
            .map_err(|e| format!("Failed to write to stdin: {}", e))?;
        drop(stdin);

        match cmd.wait_with_output() {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("{}", e)),
        }
    }
}

async fn background_task(mut manager: SshAccessManager) {
    loop {
        if !manager.enabled.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }

        manager.check_for_keyset_changes().await;
        tokio::time::sleep(REGISTRY_CHECK_INTERVAL).await;
    }
}
