//! One-shot task admission for an inert guest.
use crate::api::{
    GuestConfig, GuestFrameRead, GuestFrameWrite, GuestTemplate, GuestTemplateProtocol,
    GuestValidation, StdinMode, TEMPLATE_INSTANCE_ENV, TEMPLATE_VERSION, TemplateReady,
    TemplateTask,
};
use std::io::{self, Read, Write};

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn valid_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl GuestTemplateProtocol for GuestTemplate {
    /// Validate before any guest setup or readiness output. Failure has no effects.
    fn validate(&self) -> io::Result<()> {
        if self.version != TEMPLATE_VERSION || !valid_id(&self.template_id) {
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
    fn receive(
        &self,
        input: &mut impl Read,
        output: &mut impl Write,
        reseed: impl FnOnce(&[u8; 32]) -> io::Result<()>,
    ) -> io::Result<GuestConfig> {
        self.validate()?;
        output.write_frame(&TemplateReady {
            version: TEMPLATE_VERSION,
            template_id: self.template_id.clone(),
        })?;
        output.flush()?;
        let mut task: TemplateTask = input.read_frame()?;
        if task.version != TEMPLATE_VERSION
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
        if config.root_disk.is_some()
            || config.env.contains_key(TEMPLATE_INSTANCE_ENV)
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
        config.validate()?;
        reseed(&task.entropy)?;
        config
            .env
            .insert(TEMPLATE_INSTANCE_ENV.into(), task.instance_id);
        Ok(task.config)
    }
}
