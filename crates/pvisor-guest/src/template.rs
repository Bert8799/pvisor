//! One-shot task handoff for a credential-free guest template.
//!
//! This is a guest protocol, not a snapshot format or a host restore API. The
//! host must bind the template ID to its immutable image, firmware and machine
//! topology, deliver fresh entropy after each restore, and destroy a consumed
//! instance rather than returning it to a pool. No workload runs before handoff.
#![deny(missing_docs)]

use crate::{GuestConfig, StdinMode, control};
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};

/// Current template handoff protocol version. Unknown versions fail closed.
pub const VERSION: u32 = 1;
/// Reserved environment key populated from the host's new instance identity.
pub const INSTANCE_ENV: &str = "PVISOR_INSTANCE_ID";

/// Alternative boot document at `/.pvisor-guest.json`.
///
/// Ordinary launch documents remain unchanged. A template document has exactly
/// one `template` field, with no argv, workload environment or credentials.
#[derive(Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum BootConfig {
    /// Existing immediate workload launch.
    Workload(Box<GuestConfig>),
    /// Initialize only the guest platform, then await one framed task on stdin.
    Template {
        /// Versioned, non-secret template identity and console topology.
        template: Template,
    },
}

/// Identity of an inert template. This document contains no per-instance data.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Template {
    /// Must equal [`VERSION`].
    pub version: u32,
    /// Host-defined content identity: exactly 64 lowercase hexadecimal digits.
    /// The guest checks equality, not the underlying image digest.
    pub template_id: String,
    /// The existing stdin/stdout/stderr virtio-console topology. Input and output
    /// ports are required for the handoff; stderr may use the ordinary console.
    pub stdio_ports: [bool; 3],
}

/// A template's handoff listener is ready, not an application readiness signal.
///
/// The host may capture an otherwise eligible VM after receiving this frame.
/// Device freeze, transport state and snapshot compatibility remain VMM duties.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ready {
    /// Protocol version.
    pub version: u32,
    /// Template for which the guest is awaiting a task.
    pub template_id: String,
}

/// Exactly one task, sent only after the host has restored an independent VM.
///
/// Framing uses [`control::write_frame`] and its 1 MiB encoded limit. EOF,
/// truncation, invalid data or failed reseeding aborts this instance; callers
/// must not retry a partial handoff. These bytes can contain workload secrets.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    /// Must equal [`VERSION`].
    pub version: u32,
    /// Must match the inert template's identity exactly.
    pub template_id: String,
    /// Fresh host-assigned instance identity, 1..=128 ASCII letters, digits,
    /// hyphens or underscores; UUIDs and daemon sandbox IDs are accepted.
    /// The host must ensure uniqueness; syntax alone is not a uniqueness proof.
    pub instance_id: String,
    /// Fresh bytes from the host CSPRNG, generated after restoring this instance.
    /// A deterministic, repeated or template-captured seed violates the contract.
    pub entropy: [u8; 32],
    /// Complete launch options, replacing any previous task's options. Initial
    /// stdin must be Closed and PTY disabled: stdin carries this handoff only.
    /// `PVISOR_INSTANCE_ID` is reserved; host env is never inherited.
    pub config: GuestConfig,
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn valid_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl Template {
    /// Validate before any guest setup or readiness output. Failure has no effects.
    pub fn validate(&self) -> io::Result<()> {
        if self.version != VERSION || !valid_id(&self.template_id) {
            return Err(invalid("invalid template version or identity"));
        }
        if !self.stdio_ports[0] || !self.stdio_ports[1] {
            return Err(invalid("template handoff requires stdin and stdout ports"));
        }
        Ok(())
    }

    /// Advertise handoff readiness, read and validate exactly one task, then
    /// refresh the guest random generator before returning its launch options.
    ///
    /// Call once after platform initialization, with private host-owned console
    /// streams. This synchronous operation blocks without polling or worker
    /// threads; the host owns timeout, cancellation and VM teardown. No task
    /// process or sidecar is started here. Invalid tasks never call `reseed`;
    /// reseed failure returns no launch options. This consumes one frame only;
    /// the supervisor never accepts a second task or reuses workload stdin.
    pub fn receive(
        &self,
        input: &mut impl Read,
        output: &mut impl Write,
        reseed: impl FnOnce(&[u8; 32]) -> io::Result<()>,
    ) -> io::Result<GuestConfig> {
        self.validate()?;
        control::write_frame(
            output,
            &Ready {
                version: VERSION,
                template_id: self.template_id.clone(),
            },
        )?;
        output.flush()?;
        let mut task: Task = control::read_frame(input)?;
        if task.version != VERSION
            || task.template_id != self.template_id
            || task.instance_id.is_empty()
            || task.instance_id.len() > 128
            || !task
                .instance_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            || task.entropy == [0; 32]
        {
            return Err(invalid(
                "invalid template task identity, version or entropy",
            ));
        }
        let config = &mut task.config;
        if config.env.contains_key(INSTANCE_ENV)
            || config
                .stdio_ports
                .is_some_and(|ports| ports != self.stdio_ports)
            || !config.process.as_ref().is_some_and(|process| {
                matches!(process.stdin, StdinMode::Closed) && process.tty.is_none()
            })
        {
            return Err(invalid(
                "template task conflicts with handoff console or identity",
            ));
        }
        config.command()?;
        reseed(&task.entropy)?;
        config.env.insert(INSTANCE_ENV.into(), task.instance_id);
        Ok(task.config)
    }
}
