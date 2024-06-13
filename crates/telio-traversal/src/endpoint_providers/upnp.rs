use crate::endpoint_providers::{
    EndpointCandidate, EndpointCandidatesChangeEvent, EndpointProvider, EndpointProviderType,
    Error, PongEvent,
};
use crate::ping_pong_handler::PingPongHandler;
use async_trait::async_trait;
use futures::future::pending;
use futures::prelude::*;
use igd::{
    aio::{search_gateway, Gateway},
    PortMappingProtocol,
};
use ipnet::Ipv4Net;
use rand::Rng;
use rupnp::Error::HttpErrorCode;
use std::fmt::Debug;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use telio_crypto::PublicKey;
use telio_proto::{Session, WGPort};
use telio_sockets::External;
use telio_task::{io::chan::Tx, task_exec, BoxAction, Runtime, Task};
use telio_utils::{
    exponential_backoff::{Backoff, ExponentialBackoff, ExponentialBackoffBounds},
    telio_log_debug, telio_log_info, PinnedSleep,
};
use telio_wg::{DynamicWg, WireGuard};
use tokio::{net::UdpSocket, pin, sync::Mutex};

#[cfg(test)]
use mockall::automock;

const MAX_SUPPORTED_PACKET_SIZE: usize = 1500;
const GET_INTERFACE_TIMEOUT_S: Duration = Duration::from_secs(2);

type Result<T> = std::result::Result<T, Error>;

#[cfg_attr(test, automock)]
#[async_trait]
pub trait UpnpEpCommands: Send + Default + 'static {
    async fn check_endpoint_routes(&self, proxy_port: u16, wg_port: u16) -> Result<bool>;
    async fn add_any_endpoint_routes(
        &self,
        ip_addr: Ipv4Addr,
        proxy_port: u16,
        wg_port: u16,
    ) -> Result<(u16, u16)>;
    async fn extend_endpoint_duration(
        &self,
        ip_addr: Ipv4Addr,
        proxy_port: PortMapping,
        wg_port: PortMapping,
    ) -> Result<()>;
    async fn delete_endpoint_routes(&self, proxy_port: u16, wg_port: u16) -> Result<()>;
    async fn get_external_ip(&self) -> Result<Ipv4Addr>;
    async fn ensure_igd_gateway(&mut self) -> Result<()>;
    fn has_igd_gateway(&self) -> bool;
    fn drop_igd_gateway(&mut self);
}

type GatewaySearch = Pin<Box<dyn Future<Output = Result<Gateway>> + Send + Sync + 'static>>;

#[derive(Default)]
pub struct IgdGateway {
    search: Option<GatewaySearch>,
    gw: Option<Gateway>,
}

impl Debug for IgdGateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IgdGateway")
            .field("search", &self.search.is_some())
            .field("gw", &self.gw)
            .finish()
    }
}

#[async_trait]
impl UpnpEpCommands for IgdGateway {
    async fn check_endpoint_routes(&self, proxy_port: u16, wg_port: u16) -> Result<bool> {
        let mut i = 0;
        let mut map_ok: (bool, bool) = (false, false);
        let mut extend_timeout: bool = false;
        let gw = match &self.gw {
            Some(gw) => gw,
            None => return Err(Error::NoIGDGateway),
        };

        while let Ok(resp) = gw.get_generic_port_mapping_entry(i).await {
            i += 1;

            if resp.lease_duration < 10 * 60 {
                extend_timeout = true;
            }

            if resp.internal_port == proxy_port {
                map_ok.0 = true;
            } else if resp.internal_port == wg_port {
                map_ok.1 = true;
            }

            if map_ok == (true, true) {
                return Ok(extend_timeout);
            }
        }
        Err(Error::IGDError(
            igd::GetGenericPortMappingEntryError::SpecifiedArrayIndexInvalid,
        ))
    }

    async fn extend_endpoint_duration(
        &self,
        ip_addr: Ipv4Addr,
        proxy_port: PortMapping,
        wg_port: PortMapping,
    ) -> Result<()> {
        let gw = match &self.gw {
            Some(gw) => gw,
            None => return Err(Error::NoIGDGateway),
        };
        gw.add_port(
            PortMappingProtocol::UDP,
            proxy_port.external,
            SocketAddrV4::new(ip_addr, proxy_port.internal),
            3600,
            "libminiupnp",
        )
        .await?;
        gw.add_port(
            PortMappingProtocol::UDP,
            wg_port.external,
            SocketAddrV4::new(ip_addr, wg_port.internal),
            3600,
            "libminiupnp",
        )
        .await?;
        Ok(())
    }

    async fn add_any_endpoint_routes(
        &self,
        ip_addr: Ipv4Addr,
        proxy_port: u16,
        wg_port: u16,
    ) -> Result<(u16, u16)> {
        let mut new_ext_port: (u16, u16) = (0, 0);
        let gw = match &self.gw {
            Some(gw) => gw,
            None => return Err(Error::NoIGDGateway),
        };
        new_ext_port.0 = gw
            .add_any_port(
                PortMappingProtocol::UDP,
                SocketAddrV4::new(ip_addr, proxy_port),
                3600,
                "libminiupnp",
            )
            .await?;
        new_ext_port.1 = gw
            .add_any_port(
                PortMappingProtocol::UDP,
                SocketAddrV4::new(ip_addr, wg_port),
                3600,
                "libminiupnp",
            )
            .await?;
        Ok(new_ext_port)
    }

    async fn delete_endpoint_routes(&self, proxy_port: u16, wg_port: u16) -> Result<()> {
        let gw = match &self.gw {
            Some(gw) => gw,
            None => return Err(Error::NoIGDGateway),
        };
        gw.remove_port(PortMappingProtocol::UDP, wg_port).await?;
        gw.remove_port(PortMappingProtocol::UDP, proxy_port).await?;
        Ok(())
    }

    async fn ensure_igd_gateway(&mut self) -> Result<()> {
        if self.gw.is_some() {
            return Ok(());
        }

        if self.search.is_none() {
            self.search = Some(Box::pin(
                search_gateway(Default::default()).map_err(|e| e.into()),
            ));
        }

        if let Some(search) = &mut self.search {
            let result = search.await;
            self.search = None;
            self.gw = Some(result?);
        }

        Ok(())
    }

    async fn get_external_ip(&self) -> Result<Ipv4Addr> {
        let gw = match &self.gw {
            Some(gw) => gw,
            None => return Err(Error::NoIGDGateway),
        };

        match gw.get_external_ip().await {
            Ok(ip) => Ok(ip),
            Err(_e) => Err(Error::NoIGDGateway),
        }
    }

    fn has_igd_gateway(&self) -> bool {
        self.gw.is_some()
    }

    fn drop_igd_gateway(&mut self) {
        self.gw = None;
    }
}

pub struct UpnpEndpointProvider<
    Wg: WireGuard = DynamicWg,
    I: UpnpEpCommands = IgdGateway,
    E: Backoff = ExponentialBackoff,
> {
    task: Task<State<Wg, I, E>>,
}

#[derive(Default, Clone, Copy)]
pub struct PortMapping {
    pub internal: u16,
    pub external: u16,
}

/// Get interface's IP that was used to access an IGD device
fn get_my_local_endpoints(igd_ip: Ipv4Addr) -> std::result::Result<Ipv4Addr, Error> {
    let shared_range: Ipv4Net = Ipv4Net::new(Ipv4Addr::new(100, 64, 0, 0), 10)?;
    let igd_subnet: Ipv4Net = Ipv4Net::new(igd_ip, 16)?;

    if_addrs::get_if_addrs()?
        .iter()
        .find_map(|ip| {
            if ip.addr.is_loopback() {
                return None;
            }
            match ip.addr.ip() {
                IpAddr::V4(v4) => {
                    if shared_range.contains(&v4) && !igd_subnet.contains(&v4) {
                        None
                    } else {
                        Some(v4)
                    }
                }
                // Filter IPv6
                _ => None,
            }
        })
        .ok_or(Error::NoMatchingLocalEndpoint)
}

impl<Wg: WireGuard> UpnpEndpointProvider<Wg> {
    pub fn start(
        udp_socket: External<UdpSocket>,
        wg: Arc<Wg>,
        exponential_backoff_bounds: ExponentialBackoffBounds,
        ping_pong_handler: Arc<Mutex<PingPongHandler>>,
        is_battery_optimization_on: bool,
    ) -> Result<Self> {
        Ok(Self::start_with(
            udp_socket,
            wg,
            ExponentialBackoff::new(exponential_backoff_bounds)?,
            ping_pong_handler,
            IgdGateway::default(),
            is_battery_optimization_on,
        ))
    }
}

impl<Wg: WireGuard, I: UpnpEpCommands, E: Backoff> UpnpEndpointProvider<Wg, I, E> {
    pub fn start_with(
        udp_socket: External<UdpSocket>,
        wg: Arc<Wg>,
        exponential_backoff: E,
        ping_pong_handler: Arc<Mutex<PingPongHandler>>,
        igd_gw: I,
        is_battery_optimization_on: bool,
    ) -> Self {
        let udp_socket = Arc::new(udp_socket);
        let rx_buff = vec![0u8; MAX_SUPPORTED_PACKET_SIZE];
        let initial_upnp_interval = exponential_backoff.get_backoff();

        Self {
            task: Task::start(State {
                udp_socket,
                ip_addr: Ipv4Addr::new(0, 0, 0, 0),
                wg,
                proxy_port_mapping: PortMapping::default(),
                wg_port_mapping: PortMapping::default(),
                endpoint_candidate: None,
                pong_events_tx: None,
                epc_event_tx: None,
                exponential_backoff,
                upnp_interval: PinnedSleep::new(initial_upnp_interval, ()),
                is_battery_optimization_on,
                is_endpoint_provider_paused: false,
                rx_buff,
                igd_gw,
                ping_pong_handler,
            }),
        }
    }

    pub async fn get_endpoint_candidate(&self) -> Option<EndpointCandidate> {
        task_exec!(&self.task, async move |s| {
            let _ = s.check_endpoint_candidate().await;
            Ok(s.endpoint_candidate.clone())
        })
        .await
        .unwrap_or(None)
    }

    pub async fn get_internal_socket(&self) -> Option<SocketAddr> {
        task_exec!(&self.task, async move |s| {
            if s.endpoint_candidate.is_some() {
                Ok(Some(SocketAddr::new(
                    IpAddr::V4(s.ip_addr),
                    s.proxy_port_mapping.internal,
                )))
            } else {
                Ok(None)
            }
        })
        .await
        .unwrap_or(None)
    }

    pub async fn stop(self) {
        let _ = task_exec!(&self.task, async move |s| {
            if s.endpoint_candidate.is_some() {
                let delete = s.igd_gw.delete_endpoint_routes(
                    s.proxy_port_mapping.external,
                    s.wg_port_mapping.external,
                );
                let _ = delete.await;
            };
            Ok(())
        })
        .await;
        let _ = self.task.stop().await.resume_unwind();
    }
}

#[async_trait]
impl<Wg: WireGuard> EndpointProvider for UpnpEndpointProvider<Wg> {
    fn name(&self) -> &'static str {
        "UPnP"
    }

    async fn subscribe_for_pong_events(&self, tx: Tx<PongEvent>) {
        task_exec!(&self.task, async move |s| {
            s.pong_events_tx = Some(tx);
            Ok(())
        })
        .await
        .unwrap_or_default()
    }

    async fn subscribe_for_endpoint_candidates_change_events(
        &self,
        tx: Tx<EndpointCandidatesChangeEvent>,
    ) {
        task_exec!(&self.task, async move |s| {
            s.epc_event_tx = Some(tx);
            Ok(())
        })
        .await
        .unwrap_or_default()
    }

    async fn trigger_endpoint_candidates_discovery(&self, _force: bool) -> Result<()> {
        let _ = task_exec!(&self.task, async move |s| {
            let _ = s.send_endpoint_candidate().await;
            Ok(())
        })
        .await;
        Ok(())
    }

    async fn handle_endpoint_gone_notification(&self) {
        task_exec!(&self.task, async move |s| {
            s.endpoint_candidate = None;
            Ok(())
        })
        .await
        .unwrap_or_default()
    }

    async fn send_ping(
        &self,
        addr: SocketAddr,
        session_id: Session,
        public_key: PublicKey,
    ) -> Result<()> {
        task_exec!(&self.task, async move |s| Ok(s
            .send_ping(addr, session_id, &public_key)
            .await))
        .await?
    }

    async fn get_current_endpoints(&self) -> Option<Vec<EndpointCandidate>> {
        task_exec!(&self.task, async move |s| {
            if let Some(candidate) = s.endpoint_candidate.clone() {
                return Ok(Some(vec![candidate]));
            }
            Ok(None)
        })
        .await
        .unwrap_or(None)
    }

    async fn pause(&self) {
        let _ = task_exec!(&self.task, async move |s| {
            if s.is_battery_optimization_on {
                s.is_endpoint_provider_paused = true;
            }
            Ok(())
        })
        .await;
    }

    async fn unpause(&self) {
        let _ = task_exec!(&self.task, async move |s| {
            s.is_endpoint_provider_paused = false;
            Ok(())
        })
        .await;
    }

    async fn is_paused(&self) -> bool {
        task_exec!(&self.task, async move |s| Ok(s.is_endpoint_provider_paused))
            .await
            .unwrap_or(false)
    }
}

struct State<Wg: WireGuard, I: UpnpEpCommands, E: Backoff> {
    udp_socket: Arc<External<UdpSocket>>,
    ip_addr: Ipv4Addr,
    wg: Arc<Wg>,
    proxy_port_mapping: PortMapping,
    wg_port_mapping: PortMapping,
    endpoint_candidate: Option<EndpointCandidate>,
    pong_events_tx: Option<Tx<PongEvent>>,
    epc_event_tx: Option<Tx<EndpointCandidatesChangeEvent>>,
    exponential_backoff: E,
    upnp_interval: PinnedSleep<()>,
    is_battery_optimization_on: bool,
    is_endpoint_provider_paused: bool,
    rx_buff: Vec<u8>,
    igd_gw: I,
    ping_pong_handler: Arc<Mutex<PingPongHandler>>,
}

impl<Wg: WireGuard, I: UpnpEpCommands, E: Backoff> State<Wg, I, E> {
    async fn send_ping(
        &self,
        addr: SocketAddr,
        session_id: Session,
        public_key: &PublicKey,
    ) -> Result<()> {
        let wg_port = if let Some(port) = self.get_wg_port() {
            port
        } else {
            return Err(Error::NoWGListenPort);
        };
        self.ping_pong_handler
            .lock()
            .await
            .send_ping(addr, wg_port, &self.udp_socket, session_id, public_key)
            .await
    }

    /// Get wg port identified by stun
    fn get_wg_port(&self) -> Option<WGPort> {
        if let Some(epc) = &self.endpoint_candidate {
            return Some(WGPort(epc.wg.port()));
        }
        None
    }

    async fn check_endpoint_candidate(&mut self) -> Result<()> {
        if self.endpoint_candidate.is_none() {
            self.igd_gw.drop_igd_gateway();

            return Err(Error::NoIGDGateway);
        };

        match self.is_current_endpoint_valid().await {
            Ok(epc_extend) => {
                if epc_extend {
                    telio_log_info!(
                        "Upnp lease duration is less than 10 minutes. Extending duration."
                    );
                    let extend = self.igd_gw.extend_endpoint_duration(
                        self.ip_addr,
                        self.proxy_port_mapping,
                        self.wg_port_mapping,
                    );
                    extend.await?;
                } else {
                    telio_log_info!("The current Upnp endpoint is valid.");
                }
            }
            Err(e) => {
                if let Error::UpnpError(HttpErrorCode(http::StatusCode::INTERNAL_SERVER_ERROR)) = e
                {
                    telio_log_info!("Failed to check route: route does not exist")
                } else {
                    telio_log_info!("Error: {}", e);

                    telio_log_info!("Deleting a old Upnp endpoint");
                    let detete = self.igd_gw.delete_endpoint_routes(
                        self.proxy_port_mapping.external,
                        self.wg_port_mapping.external,
                    );
                    let _ = detete.await;
                }

                telio_log_info!("Creating a new Upnp endpoint");
                self.igd_gw.drop_igd_gateway();

                return Err(Error::NoIGDGateway);
            }
        };
        Ok(())
    }

    async fn is_current_endpoint_valid(&mut self) -> Result<bool> {
        let check = self.igd_gw.check_endpoint_routes(
            self.proxy_port_mapping.internal,
            self.wg_port_mapping.internal,
        );
        check.await
    }

    fn create_random_endpoint_ports(&mut self) {
        self.wg_port_mapping.external = rand::thread_rng().gen_range(1024..49151);
        self.proxy_port_mapping.external = loop {
            let rand = rand::thread_rng().gen_range(1024..49151);
            if rand != self.wg_port_mapping.external {
                break rand;
            }
        };
    }

    async fn create_endpoint_candidate(&mut self) -> Result<()> {
        self.create_random_endpoint_ports();

        self.proxy_port_mapping.internal = self.udp_socket.local_addr()?.port();

        self.wg_port_mapping.internal = self
            .wg
            .wait_for_listen_port(GET_INTERFACE_TIMEOUT_S)
            .await?;

        let ext_ip = {
            let get = self.igd_gw.get_external_ip();
            get.await?
        };
        self.ip_addr = get_my_local_endpoints(ext_ip)?;

        (
            self.proxy_port_mapping.external,
            self.wg_port_mapping.external,
        ) = {
            let add = self.igd_gw.add_any_endpoint_routes(
                self.ip_addr,
                self.proxy_port_mapping.internal,
                self.wg_port_mapping.internal,
            );
            add.await?
        };

        self.endpoint_candidate = Some(EndpointCandidate {
            wg: SocketAddr::new(IpAddr::V4(ext_ip), self.wg_port_mapping.external),
            udp: SocketAddr::new(IpAddr::V4(ext_ip), self.proxy_port_mapping.external),
        });

        if let Some(epc) = self.endpoint_candidate.clone() {
            if let Some(epc_tx) = &self.epc_event_tx {
                telio_log_debug!("Got UpnpEndpointCanditate: {:?}", epc);
                let _ = epc_tx.send((EndpointProviderType::Upnp, vec![epc])).await;
            }
        }
        Ok(())
    }

    async fn send_endpoint_candidate(&self) {
        if let Some(epc) = self.endpoint_candidate.clone() {
            if let Some(epc_tx) = &self.epc_event_tx {
                let _ = epc_tx.send((EndpointProviderType::Upnp, vec![epc])).await;
            }
        }
    }

    async fn handle_ping_rx(&mut self, encrypted_buf: &[u8], addr: &SocketAddr) -> Result<()> {
        let wg_port = if let Some(port) = self.get_wg_port() {
            port
        } else {
            return Err(Error::NoWGListenPort);
        };

        self.ping_pong_handler
            .lock()
            .await
            .handle_rx_packet(
                encrypted_buf,
                addr,
                wg_port,
                &self.udp_socket,
                &self.pong_events_tx,
                telio_model::features::EndpointProvider::Upnp,
            )
            .await
    }
}

#[async_trait]
impl<Wg: WireGuard, I: UpnpEpCommands, E: Backoff> Runtime for State<Wg, I, E> {
    const NAME: &'static str = "UpnpEndpointProvider";

    type Err = ();

    #[allow(index_access_check)]
    async fn wait_with_update<F>(&mut self, updated: F) -> std::result::Result<(), Self::Err>
    where
        F: Future<Output = BoxAction<Self, std::result::Result<(), Self::Err>>> + Send,
    {
        pin!(updated);

        if self.is_endpoint_provider_paused {
            telio_log_debug!("Skipping getting endpoint via UPNP endpoint provider(ModulePaused)");
            tokio::select! {
                _ = pending() => {},
                update = &mut updated => {
                    return update(self).await;
                }
            }
            return Ok(());
        }

        tokio::select! {
            Ok((len, addr)) = self.udp_socket.recv_from(&mut self.rx_buff) => {
                let buff = self.rx_buff.clone();
                let _ = self.handle_ping_rx(&buff[..len], &addr).await;
            }
            result = self.igd_gw.ensure_igd_gateway(), if !self.igd_gw.has_igd_gateway() => {
                if result.is_ok() {
                    if self.create_endpoint_candidate().await.is_err() {
                        self.igd_gw.drop_igd_gateway();
                    }
                    self.exponential_backoff.reset();
                    self.upnp_interval =  PinnedSleep::new(self.exponential_backoff.get_backoff(), ());
                }
            }
            _ = &mut self.upnp_interval => {
                self.upnp_interval =  PinnedSleep::new(self.exponential_backoff.get_backoff(), ());

                match self.check_endpoint_candidate().await {
                    Ok (_) => self.exponential_backoff.reset(),
                    Err(_) => self.exponential_backoff.next_backoff(),
                }
            }
            // Incoming task
            update = updated => {
                return update(self).await;
            }
            else => {
                return Ok(());
            },
        };

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::endpoint_providers::upnp::{
        async_trait, EndpointCandidate, MockUpnpEpCommands, UpnpEndpointProvider,
    };
    use crate::endpoint_providers::Error;
    use crate::ping_pong_handler::PingPongHandler;
    use lazy_static::lazy_static;
    use mockall::mock;
    use parking_lot::Mutex;
    use std::cell::RefCell;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::rc::Rc;
    use std::sync::Arc;
    use std::time::Duration;
    use telio_crypto::PublicKey;
    use telio_crypto::SecretKey;
    use telio_sockets::{NativeProtector, SocketPool};
    use telio_utils::exponential_backoff::MockBackoff;
    use telio_wg::uapi::{Interface, Peer};
    use telio_wg::Error as wgError;
    use telio_wg::WireGuard;
    use tokio::sync::Mutex as TMutex;

    type Result<T> = std::result::Result<T, Error>;
    type Result1<T> = std::result::Result<T, wgError>;

    mock! {
        pub Wg {}
        #[async_trait]
        impl WireGuard for Wg {
            async fn get_interface(&self) -> Result1<Interface,>;
            async fn get_adapter_luid(&self) -> Result1<u64>;
            async fn wait_for_listen_port(&self, d: Duration) -> Result1<u16>;
            async fn wait_for_proxy_listen_port(&self, d: Duration) -> Result1<u16>;
            async fn get_wg_socket(&self, ipv6: bool) -> Result1<Option<i32>>;
            async fn set_secret_key(&self, key: SecretKey) -> Result1<()>;
            async fn set_fwmark(&self, fwmark: u32) -> Result1<()>;
            async fn add_peer(&self, peer: Peer) -> Result1<()>;
            async fn del_peer(&self, key: PublicKey) -> Result1<()>;
            async fn drop_connected_sockets(&self) -> Result1<()>;
            async fn time_since_last_rx(&self, public_key: PublicKey) -> Result1<Option<Duration>>;
            async fn time_since_last_endpoint_change(&self, public_key: PublicKey) -> Result1<Option<Duration>>;
            async fn stop(self);
            async fn reset_existing_connections(&self, exit_pubkey: PublicKey, exit_ipv4: Ipv4Addr) -> Result1<()>;
        }
    }

    lazy_static! {
        static ref SEQUENTIAL_LOCK: Arc<TMutex<bool>> = Arc::new(TMutex::new(true));
        static ref IGD_IS_AVAILABLE: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
        static ref ENDPOINT: Arc<Mutex<EndpointCandidate>> =
            Arc::new(Mutex::new(EndpointCandidate {
                wg: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)), 1000),
                udp: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(2, 2, 2, 2)), 2000),
            }));
    }

    fn check_endpoint_route(proxy: u16, wg: u16) -> Result<bool> {
        if ENDPOINT.lock().wg.port() == proxy && ENDPOINT.lock().udp.port() == wg {
            return Ok(false);
        }
        Err(Error::FailedToGetUpnpService)
    }

    fn add_any_endpoint(wg: u16, proxy: u16) -> Result<(u16, u16)> {
        let mut epc = ENDPOINT.lock();
        epc.wg.set_port(wg);
        epc.udp.set_port(proxy);
        Ok((0, 0))
    }

    fn delete_endpoint_routes() -> Result<()> {
        let mut epc = ENDPOINT.lock();
        epc.wg.set_port(0);
        epc.udp.set_port(0);
        Ok(())
    }

    fn search_igd_gateway() -> Result<()> {
        *IGD_IS_AVAILABLE.lock() = true;
        Ok(())
    }

    fn get_external_ip() -> Result<Ipv4Addr> {
        Ok(Ipv4Addr::new(0, 0, 0, 0))
    }

    fn has_igd_gateway() -> bool {
        *IGD_IS_AVAILABLE.lock()
    }

    fn drop_igd_gateway() {
        *IGD_IS_AVAILABLE.lock() = false;
    }

    pub async fn prepare_test_setup(
    ) -> UpnpEndpointProvider<MockWg, MockUpnpEpCommands, MockBackoff> {
        let spool = SocketPool::new(
            NativeProtector::new(
                #[cfg(target_os = "macos")]
                false,
            )
            .unwrap(),
        );
        let udp_socket = spool
            .new_external_udp((Ipv4Addr::UNSPECIFIED, 0), None)
            .await
            .unwrap();

        // Set to default ports
        let mut epc = ENDPOINT.lock();
        epc.wg.set_port(1000);
        epc.udp.set_port(2000);

        // These are not properly used yet, just dummy variables
        let mut wg = MockWg::default();
        let wg_port = 12345;
        let wg_peers = Vec::<(PublicKey, Peer)>::new();
        let backoff_array = [100, 200, 400, 800, 1600, 3200];
        wg.expect_get_interface().returning(move || {
            Ok(Interface {
                listen_port: Some(wg_port),
                peers: wg_peers.clone().into_iter().collect(),
                ..Default::default()
            })
        });
        wg.expect_wait_for_listen_port()
            .returning(move |_| Ok(wg_port));

        let mut mock = MockUpnpEpCommands::new();
        mock.expect_add_any_endpoint_routes()
            .returning(move |_, wg_port, proxy_port| add_any_endpoint(wg_port, proxy_port));
        mock.expect_delete_endpoint_routes()
            .returning(move |_, _| delete_endpoint_routes());
        mock.expect_has_igd_gateway().returning(has_igd_gateway);
        mock.expect_drop_igd_gateway().returning(drop_igd_gateway);
        mock.expect_ensure_igd_gateway()
            .returning(search_igd_gateway);
        mock.expect_get_external_ip().returning(get_external_ip);
        mock.expect_check_endpoint_routes()
            .returning(check_endpoint_route);

        UpnpEndpointProvider::start_with(
            udp_socket,
            Arc::new(wg),
            {
                let mut result = MockBackoff::default();
                let backoff_array_idx = Rc::new(RefCell::new(0));
                let backoff_array_idx_a = backoff_array_idx.clone();
                result.expect_get_backoff().returning_st(move || {
                    Duration::from_millis(backoff_array[*backoff_array_idx_a.borrow()])
                });
                let backoff_array_idx_b = backoff_array_idx.clone();
                result.expect_next_backoff().returning_st(move || {
                    backoff_array_idx_b.replace_with(|idx| *idx + 1);
                });
                result.expect_reset().returning_st(move || {
                    backoff_array_idx.replace(0);
                });
                result
            },
            Arc::new(TMutex::new(PingPongHandler::new(SecretKey::gen()))),
            mock,
            false,
        )
    }

    #[tokio::test]
    async fn create_upnp_endpoint() {
        let _quard = SEQUENTIAL_LOCK.lock().await;
        let upnp = prepare_test_setup().await;

        tokio::time::sleep(Duration::from_millis(100 + 20)).await;

        let epc = upnp.get_endpoint_candidate().await;
        let sock = upnp.get_internal_socket().await;

        assert!(epc.is_some());
        assert!(sock.is_some());
    }

    #[tokio::test]
    async fn check_igd_function_execution() {
        let _quard = SEQUENTIAL_LOCK.lock().await;
        let upnp = prepare_test_setup().await;
        tokio::time::sleep(Duration::from_millis(100 + 20)).await;

        // Upnp will change the static port to a random port
        assert!(ENDPOINT.lock().wg.port() != 1000);

        // When Upnp is stopped it should set the port to 0
        let _ = upnp.stop().await;
        assert!(ENDPOINT.lock().wg.port() == 0);
    }

    #[tokio::test]
    async fn test_no_igd_on_router() {
        let _quard = SEQUENTIAL_LOCK.lock().await;
        // Avoid initial search
        *IGD_IS_AVAILABLE.lock() = true;
        let _upnp = prepare_test_setup().await;
        tokio::time::sleep(Duration::from_millis(20)).await;

        // When IGD is off the port should be unchanged
        assert!(ENDPOINT.lock().wg.port() == 1000);

        *IGD_IS_AVAILABLE.lock() = false;
        tokio::time::sleep(Duration::from_millis(100 + 20)).await;
        assert!(ENDPOINT.lock().wg.port() != 1000);
    }

    #[tokio::test]
    async fn test_upnp_route_corruption_on_router() {
        let _quard = SEQUENTIAL_LOCK.lock().await;
        let _upnp = prepare_test_setup().await;
        tokio::time::sleep(Duration::from_millis(100 + 20)).await;

        // Upnp will change the static port to a random port
        assert!(ENDPOINT.lock().wg.port() != 1000);

        // Save current route port and corrupt it
        let old_port = ENDPOINT.lock().wg.port();
        ENDPOINT.lock().wg.set_port(10123);
        assert!(ENDPOINT.lock().wg.port() == 10123);

        // Wait Upnp to invalidate corrupted port
        tokio::time::sleep(Duration::from_millis(200 + 20)).await;
        assert!(ENDPOINT.lock().wg.port() == old_port);
    }
}
