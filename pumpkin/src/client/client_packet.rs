use num_traits::FromPrimitive;
use pumpkin_config::{ADVANCED_CONFIG, BASIC_CONFIG};
use pumpkin_core::text::TextComponent;
use pumpkin_protocol::{
    client::{
        config::{CConfigAddResourcePack, CFinishConfig, CKnownPacks, CRegistryData},
        login::{CLoginSuccess, CSetCompression},
        status::CPingResponse,
    },
    server::{
        config::{SAcknowledgeFinishConfig, SClientInformationConfig, SKnownPacks, SPluginMessage},
        handshake::SHandShake,
        login::{SEncryptionResponse, SLoginAcknowledged, SLoginPluginResponse, SLoginStart},
        status::{SStatusPingRequest, SStatusRequest},
    },
    ConnectionState, KnownPack, CURRENT_MC_PROTOCOL,
};
use uuid::Uuid;

use crate::{
    client::authentication::{self, validate_textures, GameProfile},
    entity::player::{ChatMode, Hand},
    proxy::{bungeecord::bungeecord_login, velocity::velocity_login},
    server::{Server, CURRENT_MC_VERSION},
};

use super::{authentication::AuthError, Client, PlayerConfig};

/// Processes incoming Packets from the Client to the Server
/// Implements the `Client` Packets
/// NEVER TRUST THE CLIENT. HANDLE EVERY ERROR, UNWRAP/EXPECT
/// TODO: REMOVE ALL UNWRAPS
impl Client {
    pub fn handle_handshake(&self, handshake: SHandShake) {
        dbg!("handshake");
        let version = handshake.protocol_version.0;
        self.protocol_version
            .store(version, std::sync::atomic::Ordering::Relaxed);
        *self.server_address.lock() = handshake.server_address;

        self.connection_state.store(handshake.next_state);
        if self.connection_state.load() != ConnectionState::Status {
            let protocol = version;
            match protocol.cmp(&(CURRENT_MC_PROTOCOL as i32)) {
                std::cmp::Ordering::Less => {
                    self.kick(&format!("Client outdated ({protocol}), Server uses Minecraft {CURRENT_MC_VERSION}, Protocol {CURRENT_MC_PROTOCOL}"));
                }
                std::cmp::Ordering::Equal => {}
                std::cmp::Ordering::Greater => {
                    self.kick(&format!("Server outdated, Server uses Minecraft {CURRENT_MC_VERSION}, Protocol {CURRENT_MC_PROTOCOL}"));
                }
            }
        }
    }

    pub fn handle_status_request(&self, server: &Server, _status_request: SStatusRequest) {
        self.send_packet(&server.get_status());
    }

    pub fn handle_ping_request(&self, ping_request: SStatusPingRequest) {
        dbg!("ping");
        self.send_packet(&CPingResponse::new(ping_request.payload));
        self.close();
    }

    fn is_valid_player_name(name: &str) -> bool {
        name.len() <= 16
            && name
                .chars()
                .all(|c| c > 32_u8 as char && c < 127_u8 as char)
    }

    pub fn handle_login_start(&self, server: &Server, login_start: SLoginStart) {
        log::debug!("login start, State {:?}", self.connection_state);

        if !Self::is_valid_player_name(&login_start.name) {
            self.kick("Invalid characters in username");
            return;
        }
        // default game profile, when no online mode
        // TODO: make offline uuid
        let mut gameprofile = self.gameprofile.lock();
        let proxy = &ADVANCED_CONFIG.proxy;
        if proxy.enabled {
            if proxy.velocity.enabled {
                velocity_login(self);
            } else if proxy.bungeecord.enabled {
                match bungeecord_login(self, login_start.name) {
                    Ok((_ip, profile)) => {
                        // self.address.lock() = ip;
                        self.finish_login(&profile);
                        *gameprofile = Some(profile);
                    }
                    Err(error) => self.kick(&error.to_string()),
                }
            }
        } else {
            *gameprofile = Some(GameProfile {
                id: login_start.uuid,
                name: login_start.name,
                properties: vec![],
                profile_actions: None,
            });

            // TODO: check config for encryption
            let verify_token: [u8; 4] = rand::random();
            self.send_packet(&server.encryption_request(&verify_token, BASIC_CONFIG.online_mode));
        }
    }

    pub async fn handle_encryption_response(
        &self,
        server: &Server,
        encryption_response: SEncryptionResponse,
    ) {
        let shared_secret = server.decrypt(&encryption_response.shared_secret).unwrap();

        self.set_encryption(Some(&shared_secret))
            .unwrap_or_else(|e| self.kick(&e.to_string()));

        let mut gameprofile = self.gameprofile.lock();

        if BASIC_CONFIG.online_mode {
            match self
                .autenticate(server, &shared_secret, &gameprofile.as_ref().unwrap().name)
                .await
            {
                Ok(profile) => *gameprofile = Some(profile),
                Err(e) => {
                    self.kick(&e.to_string());
                }
            }
        }

        if let Some(profile) = gameprofile.as_ref() {
            self.finish_login(profile);
        } else {
            self.kick("No Game profile");
        }
    }

    fn finish_login(&self, profile: &GameProfile) {
        // enable compression
        if ADVANCED_CONFIG.packet_compression.enabled {
            let compression = ADVANCED_CONFIG.packet_compression.compression_info.clone();
            self.send_packet(&CSetCompression::new(compression.threshold.into()));
            self.set_compression(Some(compression));
        }

        let packet = CLoginSuccess::new(&profile.id, &profile.name, &profile.properties, false);
        self.send_packet(&packet);
    }

    async fn autenticate(
        &self,
        server: &Server,
        shared_secret: &[u8],
        username: &str,
    ) -> Result<GameProfile, AuthError> {
        if let Some(auth_client) = &server.auth_client {
            let hash = server.digest_secret(shared_secret);
            let ip = self.address.lock().ip();

            let profile = authentication::authenticate(username, &hash, &ip, auth_client).await?;
            // Check if player should join
            if let Some(actions) = &profile.profile_actions {
                if !ADVANCED_CONFIG
                    .authentication
                    .player_profile
                    .allow_banned_players
                {
                    if !actions.is_empty() {
                        self.kick("Your account can't join");
                    }
                } else {
                    for allowed in &ADVANCED_CONFIG
                        .authentication
                        .player_profile
                        .allowed_actions
                    {
                        if !actions.contains(allowed) {
                            self.kick("Your account can't join");
                        }
                    }
                }
            }
            // validate textures
            for property in &profile.properties {
                validate_textures(property, &ADVANCED_CONFIG.authentication.textures)
                    .map_err(AuthError::TextureError)?;
            }
            return Ok(profile);
        }
        Err(AuthError::MissingAuthClient)
    }

    pub fn handle_plugin_response(&self, plugin_response: SLoginPluginResponse) {
        receive_plugin_response(self, &ADVANCED_CONFIG.proxy.velocity, plugin_response);
    }

    pub fn handle_login_acknowledged(
        &self,
        server: &Server,
        _login_acknowledged: SLoginAcknowledged,
    ) {
        self.connection_state.store(ConnectionState::Config);
        self.send_packet(&server.get_branding());

        let resource_config = &ADVANCED_CONFIG.resource_pack;
        if resource_config.enabled {
            let resource_pack = CConfigAddResourcePack::new(
                Uuid::new_v3(
                    &uuid::Uuid::NAMESPACE_DNS,
                    resource_config.resource_pack_url.as_bytes(),
                ),
                &resource_config.resource_pack_url,
                &resource_config.resource_pack_sha1,
                resource_config.force,
                if !resource_config.prompt_message.is_empty() {
                    Some(TextComponent::text(&resource_config.prompt_message))
                } else {
                    None
                },
            );

            self.send_packet(&resource_pack);
        }

        // known data packs
        self.send_packet(&CKnownPacks::new(&[KnownPack {
            namespace: "minecraft",
            id: "core",
            version: "1.21",
        }]));
        dbg!("login acknowledged");
    }
    pub fn handle_client_information_config(&self, client_information: SClientInformationConfig) {
        dbg!("got client settings");
        if let (Some(main_hand), Some(chat_mode)) = (
            Hand::from_i32(client_information.main_hand.into()),
            ChatMode::from_i32(client_information.chat_mode.into()),
        ) {
            *self.config.lock() = Some(PlayerConfig {
                locale: client_information.locale,
                view_distance: client_information.view_distance,
                chat_mode,
                chat_colors: client_information.chat_colors,
                skin_parts: client_information.skin_parts,
                main_hand,
                text_filtering: client_information.text_filtering,
                server_listing: client_information.server_listing,
            });
        } else {
            self.kick("Invalid hand or chat type")
        }
    }

    pub fn handle_plugin_message(&self, plugin_message: SPluginMessage) {
        if plugin_message.channel.starts_with("minecraft:brand")
            || plugin_message.channel.starts_with("MC|Brand")
        {
            dbg!("got a client brand");
            match String::from_utf8(plugin_message.data) {
                Ok(brand) => *self.brand.lock() = Some(brand),
                Err(e) => self.kick(&e.to_string()),
            }
        }
    }

    pub fn handle_known_packs(&self, server: &Server, _config_acknowledged: SKnownPacks) {
        for registry in &server.cached_registry {
            self.send_packet(&CRegistryData::new(
                &registry.registry_id,
                &registry.registry_entries,
            ));
        }

        // We are done with configuring
        dbg!("finish config");
        self.send_packet(&CFinishConfig::new());
    }

    pub async fn handle_config_acknowledged(&self, _config_acknowledged: SAcknowledgeFinishConfig) {
        dbg!("config acknowledged");
        self.connection_state.store(ConnectionState::Play);
        self.make_player
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}
