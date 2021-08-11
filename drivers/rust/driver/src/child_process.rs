//! Module for managing running child processes

use std::io::BufRead;
use std::io::BufReader;
use std::process::Child;
use std::sync::mpsc::channel;
use std::time::Duration;

use anyhow::anyhow;
use log::{debug, error, warn};
use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessExt, Signal, System, SystemExt};

use crate::plugin_models::PactPluginManifest;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RunningPluginInfo {
  pub port: u16,
  pub server_key: String
}

/// Running child process
#[derive(Debug, Clone)]
pub struct ChildPluginProcess {
  child_pid: usize,
  manifest: PactPluginManifest,
  plugin_info: RunningPluginInfo
}

impl ChildPluginProcess {
  /// Start the child process and try read the startup JSON message from its standard output.
  pub fn new(child: Child, manifest: &PactPluginManifest) -> anyhow::Result<Self> {
    let (tx, rx) = channel();
    let manifest = manifest.clone();
    let plugin_name = manifest.name.clone();
    let child_pid = child.id();
    let child_out = child.stdout
      .ok_or_else(|| anyhow!("Could not get the child process standard output stream"))?;
    let child_err = child.stderr
      .ok_or_else(|| anyhow!("Could not get the child process standard error stream"))?;

    let name = plugin_name.clone();
    tokio::task::spawn_blocking(move || {
      let mut startup_read = false;
      let reader = BufReader::new(child_out);
      for line in reader.lines() {
        match line {
          Ok(line) => {
            debug!("Plugin({}, {}, STDOUT): {}", name, child_pid, line);
            if !startup_read {
              let line = line.trim();
              if line.starts_with("{") {
                startup_read = true;
                match serde_json::from_str::<RunningPluginInfo>(line) {
                  Ok(plugin_info) => {
                    tx.send(Ok(ChildPluginProcess {
                      child_pid: child_pid as usize,
                      manifest: manifest.clone(),
                      plugin_info
                    }))
                  }
                  Err(err) => {
                    error!("Failed to read startup info from plugin - {}", err);
                    tx.send(Err(anyhow!("Failed to read startup info from plugin - {}", err)))
                  }
                }.unwrap_or_default();
              }
            }
          }
          Err(err) => warn!("Failed to read line from child process output - {}", err)
        };
      }
    });

    tokio::task::spawn_blocking(move || {
      let reader = BufReader::new(child_err);
      for line in reader.lines() {
        match line {
          Ok(line) => debug!("Plugin({}, {}, STDERR): {}", plugin_name, child_pid, line),
          Err(err) => warn!("Failed to read line from child process output - {}", err)
        };
      }
    });

    match rx.recv_timeout(Duration::from_millis(500)) {
      Ok(result) => result,
      Err(err) => {
        error!("Timeout waiting to get plugin startup info - {}", err);
        Err(anyhow!("Plugin process did not output the correct startup message in 500 ms"))
      }
    }
  }

  /// Port the plugin is running on
  pub fn port(&self) -> u16 {
    self.plugin_info.port
  }

  /// Kill the running plugin process
  pub fn kill(&self) {
    let s = System::new();
    if let Some(process) = s.process(self.child_pid as Pid) {
      process.kill(Signal::Term);
    } else {
      warn!("Child process with PID {} was not found", self.child_pid);
    }
  }
}
