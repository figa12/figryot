use std::sync::Arc;

use async_graphql::{Error, Result as GqlResult};
use chrono::Utc;
use common_utils::ryot_log;
use database_models::prelude::{CollectionToEntity, Metadata};
use database_models::{integration, prelude::Integration};
use database_utils::user_preferences_by_id;
use dependent_models::ImportResult;
use dependent_utils::{commit_metadata, process_import};
use enums::{EntityLot, IntegrationLot, IntegrationProvider, MediaLot};
use providers::google_books::GoogleBooksService;
use rust_decimal_macros::dec;
use sea_orm::{ActiveModelTrait, ActiveValue, ColumnTrait, EntityTrait, QueryFilter, QuerySelect};
use sink::generic_json::GenericJsonIntegration;
use supporting_service::SupportingService;
use traits::TraceOk;
use uuid::Uuid;

mod push;
mod sink;
mod utils;
mod yank;

use crate::{
    push::{radarr::RadarrIntegration, sonarr::SonarrIntegration},
    sink::{
        emby::EmbyIntegration, jellyfin::JellyfinIntegration, kodi::KodiIntegration,
        plex::PlexIntegration,
    },
    yank::{audiobookshelf::AudiobookshelfIntegration, komga::KomgaIntegration},
};

pub struct IntegrationService(pub Arc<SupportingService>);

impl IntegrationService {
    async fn integration_progress_update(
        &self,
        integration: integration::Model,
        updates: ImportResult,
    ) -> GqlResult<()> {
        let mut updates = updates;
        updates.metadata.iter_mut().for_each(|media| {
            media.seen_history.retain(|update| match update.progress {
                Some(progress) if progress < integration.minimum_progress.unwrap() => {
                    ryot_log!(
                        debug,
                        "Progress update for integration {} is below minimum threshold",
                        integration.id
                    );
                    false
                }
                Some(_) => true,
                None => false,
            });
            media.seen_history.iter_mut().for_each(|update| {
                if let Some(progress) = update.progress {
                    if progress > integration.maximum_progress.unwrap() {
                        ryot_log!(
                            debug,
                            "Changing progress to 100 for integration {}",
                            integration.id
                        );
                        update.progress = Some(dec!(100));
                    }
                }
            });
        });
        if let Err(err) = process_import(&integration.user_id, updates, &self.0).await {
            ryot_log!(debug, "Error updating progress: {:?}", err);
        } else {
            let mut to_update: integration::ActiveModel = integration.into();
            to_update.last_triggered_on = ActiveValue::Set(Some(Utc::now()));
            to_update.update(&self.0.db).await?;
        }
        Ok(())
    }

    pub async fn process_integration_webhook(
        &self,
        integration_slug: String,
        payload: String,
    ) -> GqlResult<String> {
        ryot_log!(
            debug,
            "Processing integration webhook for slug: {}",
            integration_slug
        );
        let integration = Integration::find_by_id(integration_slug)
            .one(&self.0.db)
            .await?
            .ok_or_else(|| Error::new("Integration does not exist".to_owned()))?;
        let preferences = user_preferences_by_id(&integration.user_id, &self.0).await?;
        if integration.is_disabled.unwrap_or_default() || preferences.general.disable_integrations {
            return Err(Error::new("Integration is disabled".to_owned()));
        }
        let maybe_progress_update = match integration.provider {
            IntegrationProvider::Kodi => {
                let kodi = KodiIntegration::new(payload);
                kodi.yank_progress().await
            }
            IntegrationProvider::Emby => {
                let emby = EmbyIntegration::new(payload, self.0.db.clone());
                emby.yank_progress().await
            }
            IntegrationProvider::Jellyfin => {
                let jellyfin = JellyfinIntegration::new(payload);
                jellyfin.yank_progress().await
            }
            IntegrationProvider::Plex => {
                let specifics = integration.clone().provider_specifics.unwrap();
                let plex =
                    PlexIntegration::new(payload, specifics.plex_username, self.0.db.clone());
                plex.yank_progress().await
            }
            IntegrationProvider::GenericJson => {
                let generic_json = GenericJsonIntegration::new(payload);
                generic_json.yank_progress().await
            }
            _ => return Err(Error::new("Unsupported integration source".to_owned())),
        };
        match maybe_progress_update {
            Ok(pu) => {
                self.integration_progress_update(integration, pu)
                    .await
                    .trace_ok();
                Ok("Progress updated successfully".to_owned())
            }
            Err(e) => Err(Error::new(e.to_string())),
        }
    }

    pub async fn handle_entity_added_to_collection_event(
        &self,
        user_id: String,
        collection_to_entity_id: Uuid,
    ) -> GqlResult<()> {
        let cte = CollectionToEntity::find_by_id(collection_to_entity_id)
            .one(&self.0.db)
            .await?
            .ok_or_else(|| Error::new("Collection to entity does not exist"))?;
        if !matches!(cte.entity_lot, EntityLot::Metadata) {
            return Ok(());
        }
        let integrations = Integration::find()
            .filter(integration::Column::UserId.eq(user_id))
            .filter(integration::Column::Lot.eq(IntegrationLot::Push))
            .all(&self.0.db)
            .await?;
        for integration in integrations {
            let possible_collection_ids = match integration.provider_specifics.clone() {
                Some(s) => match integration.provider {
                    IntegrationProvider::Radarr => s.radarr_sync_collection_ids.unwrap_or_default(),
                    IntegrationProvider::Sonarr => s.sonarr_sync_collection_ids.unwrap_or_default(),
                    _ => vec![],
                },
                None => vec![],
            };
            if !possible_collection_ids.contains(&cte.collection_id) {
                continue;
            }
            let specifics = integration.provider_specifics.unwrap();
            let metadata = Metadata::find_by_id(&cte.entity_id)
                .one(&self.0.db)
                .await?
                .ok_or_else(|| Error::new("Metadata does not exist"))?;
            let maybe_entity_id = match metadata.lot {
                MediaLot::Show => metadata
                    .external_identifiers
                    .and_then(|ei| ei.tvdb_id.map(|i| i.to_string())),
                _ => Some(metadata.identifier.clone()),
            };
            if let Some(entity_id) = maybe_entity_id {
                let _push_result = match integration.provider {
                    IntegrationProvider::Radarr => {
                        let sonarr = SonarrIntegration::new(
                            specifics.radarr_base_url.unwrap(),
                            specifics.radarr_api_key.unwrap(),
                            specifics.radarr_profile_id.unwrap(),
                            specifics.radarr_root_folder_path.unwrap(),
                            entity_id,
                        );
                        sonarr.push_progress().await
                    }
                    IntegrationProvider::Sonarr => {
                        let radarr = RadarrIntegration::new(
                            specifics.sonarr_base_url.unwrap(),
                            specifics.sonarr_api_key.unwrap(),
                            specifics.sonarr_profile_id.unwrap(),
                            specifics.sonarr_root_folder_path.unwrap(),
                            entity_id,
                        );
                        radarr.push_progress().await
                    }
                    _ => unreachable!(),
                };
            }
        }
        Ok(())
    }

    pub async fn yank_integrations_data_for_user(&self, user_id: &String) -> GqlResult<bool> {
        let preferences = user_preferences_by_id(user_id, &self.0).await?;
        if preferences.general.disable_integrations {
            return Ok(false);
        }
        let integrations = Integration::find()
            .filter(integration::Column::UserId.eq(user_id))
            .filter(integration::Column::Lot.eq(IntegrationLot::Yank))
            .all(&self.0.db)
            .await?;
        let mut progress_updates = vec![];
        let mut to_update_integrations = vec![];
        for integration in integrations.into_iter() {
            if integration.is_disabled.unwrap_or_default() {
                ryot_log!(debug, "Integration {} is disabled", integration.id);
                continue;
            }
            let specifics = integration.clone().provider_specifics.unwrap();
            let response = match integration.provider {
                IntegrationProvider::Audiobookshelf => {
                    let audiobookshelf = AudiobookshelfIntegration::new(
                        specifics.audiobookshelf_base_url.unwrap(),
                        specifics.audiobookshelf_token.unwrap(),
                        integration.sync_to_owned_collection,
                        GoogleBooksService::new(
                            &self.0.config.books.google_books,
                            self.0.config.frontend.page_size,
                        )
                        .await,
                    );
                    audiobookshelf
                        .yank_progress(|input| commit_metadata(input, &self.0))
                        .await
                }
                IntegrationProvider::Komga => {
                    let komga = KomgaIntegration::new(
                        specifics.komga_base_url.unwrap(),
                        specifics.komga_username.unwrap(),
                        specifics.komga_password.unwrap(),
                        specifics.komga_provider.unwrap(),
                        self.0.db.clone(),
                        integration.sync_to_owned_collection,
                    );
                    komga.yank_progress().await
                }
                _ => continue,
            };
            if let Ok(update) = response {
                to_update_integrations.push(integration.id.clone());
                progress_updates.push((integration, update));
            }
        }
        for (integration, progress_updates) in progress_updates.into_iter() {
            self.integration_progress_update(integration, progress_updates)
                .await
                .trace_ok();
        }
        Ok(true)
    }

    pub async fn yank_integrations_data(&self) -> GqlResult<()> {
        let users_with_integrations = Integration::find()
            .filter(integration::Column::Lot.eq(IntegrationLot::Yank))
            .select_only()
            .column(integration::Column::UserId)
            .into_tuple::<String>()
            .all(&self.0.db)
            .await?;
        for user_id in users_with_integrations {
            ryot_log!(debug, "Yanking integrations data for user {}", user_id);
            self.yank_integrations_data_for_user(&user_id).await?;
        }
        Ok(())
    }
}
