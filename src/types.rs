pub use listenbrainz::raw::response::{UserPlayingNowListen, UserPlayingNowTrackMetadata};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct CaloreResponse {
    pub palette: Vec<[u8; 3]>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Artist {
    pub artist_credit_name: String,
    pub artist_mbid: Option<String>,
    pub join_phrase: String,
    pub url: Option<String>,
}

#[derive(Clone, Serialize, Debug)]
pub struct TransformedTrack {
    pub artists: Vec<Artist>,
    pub name: String,
    pub release: String,
    pub release_url: Option<String>,
    pub url: Option<String>,
    pub mbid: Option<String>,
    pub image_url: Option<String>,

    /// an array of RGB colors
    pub image_palette: Vec<[u8; 3]>,
}

impl TransformedTrack {
    pub fn from_listen(track: UserPlayingNowTrackMetadata) -> Self {
        let raw_urls: Vec<Value> = track
            .additional_info
            .get("spotify_artist_ids")
            .map(|value| value.as_array().unwrap().to_owned()) // if it is Some, convert it to a vec
            .unwrap_or_else(|| Vec::with_capacity(0)); // otherwise just use an empty vec

        let artist_urls: Vec<&str> = raw_urls.iter().map(|item| item.as_str().unwrap()).collect();

        let raw_mbids: Vec<Value> = track
            .additional_info
            .get("artist_mbids")
            .map(|value| value.as_array().unwrap().to_owned())
            .unwrap_or_else(|| Vec::with_capacity(0));

        let artist_mbids: Vec<String> = raw_mbids
            .iter()
            .map(|value| value.as_str().unwrap().to_owned())
            .collect();

        let artist_names: Vec<String> =
            serde_json::from_value(track.additional_info.get("artist_names").unwrap().clone())
                .unwrap();
        let artists = artist_names
            .iter()
            .enumerate()
            .map(|(i, name)| Artist {
                artist_credit_name: name.clone(),
                artist_mbid: artist_mbids.get(0).cloned(),
                join_phrase: ", ".to_string(),
                url: Some(artist_urls[i].to_string()),
            })
            .collect();

        Self {
            artists,
            name: track.track_name,
            release: track.release_name.unwrap_or_default(),
            release_url: track
                .additional_info
                .get("spotify_album_id")
                .map(|value| serde_json::from_value(value.clone()).unwrap()),
            image_url: None,
            image_palette: vec![],
            url: track
                .additional_info
                .get("origin_url")
                .map(|value| serde_json::from_value(value.clone()).unwrap()),
            mbid: track
                .additional_info
                .get("recording_mbid")
                .map(|value| serde_json::from_value(value.clone()).unwrap()),
        }
    }
}
