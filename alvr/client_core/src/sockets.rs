use alvr_common::anyhow::{Result, bail};
use mdns_sd::{ServiceDaemon, ServiceInfo};

pub struct AnnouncerSocket {
    hostname: String,
    daemon: ServiceDaemon,
    control_port: u16,
}

impl AnnouncerSocket {
    /// Announces on the well-known [`alvr_sockets::CONTROL_PORT`].
    #[allow(dead_code)] // Kept for callers that do not need a custom control port.
    pub fn new(hostname: &str) -> Result<Self> {
        Self::new_with_control_port(hostname, alvr_sockets::CONTROL_PORT)
    }

    /// Advertises a control port other than the well-known [`alvr_sockets::CONTROL_PORT`], so
    /// several emulated clients can run on one machine without colliding on it.
    pub fn new_with_control_port(hostname: &str, control_port: u16) -> Result<Self> {
        let daemon = ServiceDaemon::new()?;

        Ok(Self {
            daemon,
            hostname: hostname.to_owned(),
            control_port,
        })
    }

    pub fn announce(&self) -> Result<()> {
        let local_ip = alvr_system_info::local_ip();
        if local_ip.is_unspecified() {
            bail!("IP is unspecified");
        }

        let control_port = self.control_port.to_string();

        self.daemon.register(ServiceInfo::new(
            alvr_sockets::MDNS_SERVICE_TYPE,
            &format!("alvr{}", rand::random::<u16>()),
            &self.hostname,
            local_ip,
            5353,
            &[
                (
                    alvr_sockets::MDNS_PROTOCOL_KEY,
                    alvr_common::protocol_id().as_str(),
                ),
                (alvr_sockets::MDNS_CONTROL_PORT_KEY, control_port.as_str()),
            ][..],
        )?)?;

        Ok(())
    }
}
