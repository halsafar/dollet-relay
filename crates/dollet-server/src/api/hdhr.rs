//! HDHomeRun emulation — the path Plex actually uses.
//!
//! Thirteen routes, because a channel profile and an output profile each make
//! a distinct tuner and Plex addresses them by URL prefix. The four forms are
//! the bare one, `/{channel_profile}/`, `/output_profile/{id}/`, and both.
//!
//! Every URL in every payload here is **absolute**, built from [`Origin`].
//! Wrong values fail as "discovery works, playback doesn't": Plex finds the
//! tuner, reads a lineup, and every entry in it is unreachable.

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use dollet_core::db;
use dollet_core::domain::Id;
use dollet_core::output::hdhr;
use dollet_core::settings::{self, StreamSettings};

use super::error::ApiResult;
use super::network::NetworkGate;
use super::origin::Origin;
use crate::AppState;

/// Stand-in tuner count when a provider profile declares unlimited streams.
/// Plex will not attempt an `n+1`th recording, so this is the ceiling a user
/// with an unmetered provider gets; Xtream's stand-in is larger, because its
/// clients treat the number as a quota to display rather than tuners.
const UNLIMITED_TUNERS: u32 = 10;

/// How `/api/core/origins/` labels an address a tuner was discovered on.
const SEEN_AS: &str = "hdhr";

pub fn router() -> Router<AppState> {
    let scoped = |prefix: &str| {
        Router::new()
            .route(&format!("{prefix}/discover.json"), get(discover))
            .route(&format!("{prefix}/lineup.json"), get(lineup))
            .route(&format!("{prefix}/lineup_status.json"), get(lineup_status))
    };

    Router::new()
        .route("/hdhr/device.xml", get(device_xml))
        .merge(scoped("/hdhr"))
        .merge(scoped("/hdhr/output_profile/{output_profile}"))
        // Declared after the static `output_profile` prefix; matchit resolves
        // by specificity rather than declaration order, so the static segment
        // wins over this parameter and the two coexist.
        .merge(scoped("/hdhr/{channel_profile}"))
        .merge(scoped(
            "/hdhr/{channel_profile}/output_profile/{output_profile}",
        ))
}

/// The prefix the request arrived on, which the payloads echo back as their
/// own `BaseURL`. Rebuilt from the resolved parameters rather than read off
/// the URI so a stray `..` or double slash cannot end up advertised.
struct Scope {
    channel_profile: Option<String>,
    output_profile: Option<Id>,
}

impl Scope {
    fn from_path(path: &std::collections::HashMap<String, String>) -> Self {
        Self {
            channel_profile: path.get("channel_profile").cloned(),
            output_profile: path
                .get("output_profile")
                .and_then(|raw| raw.parse::<Id>().ok()),
        }
    }

    fn base_url(&self, origin: &Origin) -> String {
        let mut url = format!("{}/hdhr", origin.base_url());
        if let Some(profile) = &self.channel_profile {
            url.push('/');
            url.push_str(&super::urlencode(profile));
        }
        if let Some(id) = self.output_profile {
            url.push_str(&format!("/output_profile/{id}"));
        }
        url
    }

    fn identity(&self) -> hdhr::Identity {
        hdhr::Identity::new(self.channel_profile.as_deref(), self.output_profile)
    }
}

async fn device_xml(_gate: NetworkGate, origin: Origin) -> Response {
    super::origin::record(SEEN_AS, &origin);
    let scope = Scope {
        channel_profile: None,
        output_profile: None,
    };
    (
        [(axum::http::header::CONTENT_TYPE, "application/xml")],
        hdhr::device_xml(&scope.base_url(&origin), &scope.identity()),
    )
        .into_response()
}

async fn discover(
    State(state): State<AppState>,
    _gate: NetworkGate,
    origin: Origin,
    Path(path): Path<std::collections::HashMap<String, String>>,
) -> ApiResult<Json<hdhr::Discover>> {
    super::origin::record(SEEN_AS, &origin);
    let scope = Scope::from_path(&path);
    let tuners = db::m3u::tuner_count(&state.db, UNLIMITED_TUNERS).await?;
    Ok(Json(hdhr::discover(
        &scope.base_url(&origin),
        &scope.identity(),
        tuners,
    )))
}

async fn lineup_status(_gate: NetworkGate) -> Json<hdhr::LineupStatus> {
    Json(hdhr::lineup_status())
}

async fn lineup(
    State(state): State<AppState>,
    _gate: NetworkGate,
    origin: Origin,
    Path(path): Path<std::collections::HashMap<String, String>>,
) -> ApiResult<Json<Vec<hdhr::LineupEntry>>> {
    super::origin::record(SEEN_AS, &origin);
    let scope = Scope::from_path(&path);

    // A profile name that does not exist yields an empty lineup rather than a
    // 404: Plex treats a 404 as a dead tuner and stops asking, where an empty
    // lineup just shows no channels.
    let profile_id = match &scope.channel_profile {
        Some(name) => match db::channel_profiles::by_name(&state.db, name).await? {
            Some(profile) => Some(profile.id),
            None => return Ok(Json(Vec::new())),
        },
        None => None,
    };

    let channels = super::outputs::visible_channels(&state, profile_id).await?;

    // `stream_settings.hdhr_output_profile_id` is the fallback, so an operator
    // who wants Plex transcoded does not have to re-add the tuner under a
    // longer URL. The URL form still wins where both are set — it is the more
    // specific request, and the only one a second tuner can differ by.
    //
    // The advertised `BaseURL` is deliberately unchanged: the setting and the
    // URL scope are two routes to the same lineup entries, not two tuners.
    let requested = match scope.output_profile {
        Some(id) => Some(id),
        None => {
            settings::load::<StreamSettings>(&state.db)
                .await?
                .hdhr_output_profile_id
        }
    };

    // An output profile that does not exist is dropped from the URLs rather
    // than carried through, so the lineup falls back to no transcoding instead
    // of pointing every entry at a profile the stream endpoint will ignore.
    let output_profile = match requested {
        Some(id) => db::profiles::get_output_profile(&state.db, id)
            .await?
            .map(|profile| profile.id),
        None => None,
    };

    Ok(Json(hdhr::lineup(
        &origin.base_url(),
        &channels,
        output_profile,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_profile_name_is_encoded_into_the_advertised_base_url() {
        let origin = Origin {
            scheme: "http".into(),
            host: "ipx.test".into(),
            port: Some(9191),
        };

        let bare = Scope {
            channel_profile: None,
            output_profile: None,
        };
        assert_eq!(bare.base_url(&origin), "http://ipx.test:9191/hdhr");

        let both = Scope {
            channel_profile: Some("Living Room".into()),
            output_profile: Some(1),
        };
        assert_eq!(
            both.base_url(&origin),
            "http://ipx.test:9191/hdhr/Living%20Room/output_profile/1"
        );
    }

    #[test]
    fn each_scope_is_a_distinct_tuner() {
        // Plex keys a tuner on `DeviceID`, so two scopes sharing one would
        // overwrite each other in its configuration.
        let ids = [
            (None, None),
            (Some("Kids"), None),
            (None, Some(1)),
            (Some("Kids"), Some(1)),
        ]
        .map(|(profile, output)| hdhr::Identity::new(profile, output).device_id);

        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "two scopes share a DeviceID");
    }
}
