//! Wrapper around `usbip.exe` (usbip-win2). Owns command construction +
//! output parsing. The actual command runner is parameterized so tests
//! don't need a real `usbip.exe` on PATH.

use std::path::PathBuf;
use std::process::Command;

/// One exported device entry from `usbip list -r <host>`.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct RemoteDevice {
    pub busid: String,
    pub vid: String,
    pub pid: String,
}

#[derive(Debug)]
pub enum CliError {
    NotInstalled,
    InvocationFailed(String),
    ParseFailed(String),
}

/// Locate `usbip.exe`. Checks PATH first, then the default install dir.
///
/// # Errors
/// `CliError::NotInstalled` if neither location yields the binary.
pub fn locate() -> Result<PathBuf, CliError> {
    if let Ok(p) = which::which("usbip.exe") {
        return Ok(p);
    }
    let default = PathBuf::from(r"C:\Program Files\USBip\usbip.exe");
    if default.is_file() {
        return Ok(default);
    }
    Err(CliError::NotInstalled)
}

/// Parse the human-readable output of `usbip list -r <host>` into the list
/// of exported devices. The format (as of usbip-win2 0.9.7.x) is:
///
/// ```text
/// Exportable USB devices
/// ======================
///  - 192.168.1.183
///         3-3: Valve Software : unknown product (28de:1205)
///            : /sys/devices/pci0000:00/...
///            : (Defined at Interface level) (00/00/00)
/// ```
///
/// We only care about the `<busid>: ... (<vid>:<pid>)` line.
pub fn parse_list_remote(stdout: &str) -> Vec<RemoteDevice> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let trimmed = line.trim_start();
        // Heuristic: a device line has a busid prefix like "3-3:" then text
        // ending in "(xxxx:yyyy)".
        let Some((busid, rest)) = trimmed.split_once(':') else { continue };
        let busid = busid.trim();
        if busid.is_empty() || !busid.contains('-') {
            continue;
        }
        // Find the trailing "(vid:pid)" group.
        let Some(open) = rest.rfind('(') else { continue };
        let Some(close) = rest.rfind(')') else { continue };
        if close <= open + 1 {
            continue;
        }
        let inner = &rest[open + 1..close];
        let Some((vid, pid)) = inner.split_once(':') else { continue };
        out.push(RemoteDevice {
            busid: busid.to_owned(),
            vid: vid.trim().to_owned(),
            pid: pid.trim().to_owned(),
        });
    }
    out
}

/// Parse the output of `usbip port` (the local-attach list) and return the
/// set of remote busids currently attached. Format example:
///
/// ```text
/// Imported USB devices
/// ====================
/// Port 00: <Port in Use> at High Speed(480Mbps)
///        Valve Software : unknown product (28de:1205)
///        9-2 -> usbip://192.168.1.183:3240/3-3
///            -> remote bus/dev 003/006
/// ```
///
/// We extract the remote busid (the suffix after the host:port in the
/// `usbip://` URL).
pub fn parse_port(stdout: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let Some(idx) = line.find("usbip://") else { continue };
        let url = &line[idx + "usbip://".len()..];
        // After the host:port comes /<busid>; trim trailing whitespace.
        let Some(slash) = url.find('/') else { continue };
        let busid = url[slash + 1..].trim();
        if !busid.is_empty() {
            out.push(busid.to_owned());
        }
    }
    out
}

/// Production wrapper.
pub struct UsbipCli {
    path: PathBuf,
}

impl UsbipCli {
    /// Locate the binary and bind a wrapper.
    ///
    /// # Errors
    /// `CliError::NotInstalled` if `usbip.exe` isn't on PATH or in the
    /// default install dir.
    pub fn discover() -> Result<Self, CliError> {
        Ok(Self { path: locate()? })
    }

    fn run(&self, args: &[&str]) -> Result<String, CliError> {
        let out = Command::new(&self.path)
            .args(args)
            .output()
            .map_err(|e| CliError::InvocationFailed(e.to_string()))?;
        if !out.status.success() {
            return Err(CliError::InvocationFailed(format!(
                "exit {:?}: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// `usbip list -r <host>`
    ///
    /// # Errors
    /// As [`UsbipCli::run`].
    pub fn list_remote(&self, host: &str) -> Result<Vec<RemoteDevice>, CliError> {
        Ok(parse_list_remote(&self.run(&["list", "-r", host])?))
    }

    /// `usbip port`
    ///
    /// # Errors
    /// As [`UsbipCli::run`].
    pub fn port(&self) -> Result<Vec<String>, CliError> {
        Ok(parse_port(&self.run(&["port"])?))
    }

    /// `usbip attach -r <host> -b <busid>`
    ///
    /// # Errors
    /// As [`UsbipCli::run`].
    pub fn attach(&self, host: &str, busid: &str) -> Result<(), CliError> {
        self.run(&["attach", "-r", host, "-b", busid]).map(|_| ())
    }

    /// `usbip detach -p <port-num>` — used for graceful disconnect from the
    /// tray. Port number is the `Port NN` index from `usbip port` output.
    ///
    /// # Errors
    /// As [`UsbipCli::run`].
    pub fn detach(&self, port: u8) -> Result<(), CliError> {
        self.run(&["detach", "-p", &port.to_string()]).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_LIST: &str = "Exportable USB devices\n\
        ======================\n\
        - 192.168.1.183\n\
               3-3: Valve Software : unknown product (28de:1205)\n\
                  : /sys/devices/pci0000:00/0000:00:0d.0/usb3/3-3\n\
                  : (Defined at Interface level) (00/00/00)\n\
        \n";

    const SAMPLE_PORT: &str = "Imported USB devices\n\
        ====================\n\
        Port 00: <Port in Use> at High Speed(480Mbps)\n\
               Valve Software : unknown product (28de:1205)\n\
               9-2 -> usbip://192.168.1.183:3240/3-3\n\
                   -> remote bus/dev 003/006\n";

    #[test]
    fn parse_list_extracts_busid_and_vidpid() {
        let devs = parse_list_remote(SAMPLE_LIST);
        assert_eq!(devs.len(), 1);
        assert_eq!(devs[0].busid, "3-3");
        assert_eq!(devs[0].vid, "28de");
        assert_eq!(devs[0].pid, "1205");
    }

    #[test]
    fn parse_list_empty_input() {
        assert!(parse_list_remote("").is_empty());
        assert!(parse_list_remote("Exportable USB devices\n=====\n").is_empty());
    }

    #[test]
    fn parse_port_extracts_remote_busid() {
        let busids = parse_port(SAMPLE_PORT);
        assert_eq!(busids, vec!["3-3"]);
    }

    #[test]
    fn parse_port_empty_when_nothing_attached() {
        let empty = "Imported USB devices\n====================\n";
        assert!(parse_port(empty).is_empty());
    }

    #[test]
    fn parse_port_handles_multiple() {
        let two = "Imported USB devices\n\
            ====================\n\
            Port 00: <Port in Use> at High Speed(480Mbps)\n\
                   Valve Software : unknown product (28de:1205)\n\
                   9-2 -> usbip://192.168.1.183:3240/3-3\n\
            Port 01: <Port in Use> at High Speed(480Mbps)\n\
                   Other Vendor : whatever (1234:5678)\n\
                   9-3 -> usbip://192.168.1.42:3240/4-1\n";
        assert_eq!(parse_port(two), vec!["3-3", "4-1"]);
    }
}
