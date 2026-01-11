mod types;

use std::sync::Arc;
use std::time::Duration;

use crate::types::{
    CaloreResponse, TransformedTrack, UserPlayingNowListen, UserPlayingNowTrackMetadata,
};
use actix_web::{
    App, Error, HttpRequest, HttpResponse, HttpServer, Responder, middleware::Logger, rt, web,
};
use actix_ws::AggregatedMessage;
use anyhow::Result;
use env_logger::Env;
use futures_util::{FutureExt, StreamExt};
use log::{error, info};
use regex::Regex;
use reqwest::Client;
use rust_socketio::{
    Payload,
    asynchronous::{Client as SocketIOClient, ClientBuilder as SocketIOClientBuilder},
};
use serde_json::{Value, json};
use tokio::sync::watch;

struct AppState {
    rx: watch::Receiver<TransformedTrack>,
    _socketio: Arc<SocketIOClient>,
}

async fn enrich_track(client: &Client, track: &TransformedTrack) -> Result<TransformedTrack> {
    let spotify_cdn_regex = Regex::new(r"image-cdn-\w{2}\.spotifycdn\.com").unwrap();
    let image_url = if let Some(url) = &track.release_url {
        let oembed = client
            .get(format!("https://open.spotify.com/oembed?url={url}"))
            .send()
            .await?;
        oembed.error_for_status_ref()?;
        let json: Value = oembed.json::<Value>().await?;
        let thumbnail_url: String = json
            .get("thumbnail_url")
            .unwrap()
            .as_str()
            .unwrap()
            .to_string();

        // get full size art
        Some(
            spotify_cdn_regex
                .replace(&thumbnail_url, "i.scdn.co")
                .replacen("1e02", "b273", 1),
        )
    } else if let Some(mbid) = &track.mbid {
        let caa_res = client
            .get(format!("https://coverartarchive.org/release/{mbid}"))
            .send()
            .await?;
        if caa_res.error_for_status_ref().is_err() {
            None
        } else {
            Some(format!(
                "https://coverartarchive.org/release/{mbid}/front-500"
            ))
        }
    } else {
        None
    };

    let image_palette = if let Some(image_url) = &image_url {
        let calore_res = client
            .get(format!("https://calore.thrzl.xyz/?image={image_url}"))
            .send()
            .await?;
        calore_res.error_for_status_ref()?;
        let data: CaloreResponse = calore_res.json().await?;
        data.palette
    } else {
        Vec::with_capacity(0)
    };

    let mut new_track = track.clone();
    new_track.image_url = image_url;
    new_track.image_palette = image_palette;
    Ok(new_track)
}

async fn home() -> impl Responder {
    HttpResponse::Ok().body("OK\n\nconnect to ws @ /ws")
}

async fn ws_handler(
    req: HttpRequest,
    stream: web::Payload,
    state: web::Data<AppState>,
) -> Result<HttpResponse, Error> {
    let (res, mut session, stream) = actix_ws::handle(&req, stream)?;
    let mut rx = state.rx.clone();

    let mut stream = stream
        .aggregate_continuations()
        .max_continuation_size(2_usize.pow(20));

    rt::spawn(async move {
        session
            .text(serde_json::to_string(&*rx.borrow_and_update()).unwrap())
            .await
            .unwrap();

        let mut heartbeat_interval = tokio::time::interval(Duration::from_secs(5));

        let close_reason = loop {
            let tick = heartbeat_interval.tick();
            tokio::pin!(tick);

            tokio::select! {
                _ = rx.changed() => {
                    let msg = rx.borrow();
                    session
                        .text(serde_json::to_string(&*msg).unwrap())
                        .await
                        .unwrap();
                }
                msg = stream.next() => {
                    if let Some(Ok(msg)) = msg {
                        match msg {
                            AggregatedMessage::Ping(bytes) => {
                                let _ = session.pong(&bytes).await;
                            },
                            AggregatedMessage::Close(reason) => {
                                info!("socket closed");
                                break reason
                            },
                            _ => {}
                        }
                    }
                }
                _ = tick => {
                    let _ = session.ping(&[]).await;
                }
            }
        };
        let _ = session.close(close_reason).await;
    });

    // respond immediately with response connected to WS session
    Ok(res)
}

async fn get_last_track(user: &String) -> TransformedTrack {
    let client = listenbrainz::raw::Client::new();
    let now_playing_res = client.user_playing_now(user).unwrap();

    let now_playing = now_playing_res.payload.listens.get(0);

    let last_listen = match now_playing {
        Some(now_playing) => now_playing.clone().track_metadata,
        None => {
            let last_listen = client
                .user_listens(user, None, None, Some(1u64))
                .unwrap()
                .payload
                .listens[0]
                .clone();
            let metadata = last_listen.track_metadata;
            UserPlayingNowTrackMetadata {
                artist_name: metadata.artist_name,
                track_name: metadata.track_name,
                release_name: metadata.release_name,
                additional_info: metadata.additional_info,
            }
        }
    };
    let track = TransformedTrack::from_listen(last_listen);
    match enrich_track(&Client::new(), &track).await {
        Ok(track) => track,
        Err(e) => {
            error!("failed to enrich track {}: {}", track.name, e.to_string());
            track
        }
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    env_logger::init_from_env(Env::default().default_filter_or("info"));

    let _ = dotenvy::dotenv();
    let user = Arc::new(std::env::var("LISTENBRAINZ_USER").expect("USER env var should be set"));

    info!("getting initial track");
    let last_track = get_last_track(&user).await;
    info!(
        "initial track: {} - {}",
        last_track.name, last_track.artists[0].artist_credit_name
    );
    let (tx, rx) = watch::channel::<TransformedTrack>(last_track);

    info!("connecting to listenbrainz socketio server");
    let socket = {
        loop {
            let tx = tx.clone();
            let user = user.clone();
            let socket_builder = SocketIOClientBuilder::new("https://listenbrainz.org")
                .transport_type(rust_socketio::TransportType::Websocket)
                .on("open", move |_, socket| {
                    let user = user.clone();
                    info!("subscribing to user {user}");
                    async move {
                        socket
                            .emit("json", json!({"user": *user}))
                            .await
                            .expect("listenbrainz server unreachable");
                        info!("subscribed to user {user}");
                    }
                    .boxed()
                })
                .on("playing_now", move |payload, _| {
                    let tx = tx.clone();
                    let client = Client::new();
                    info!("received new track message");

                    async move {
                        let payload: Value = if let Payload::Text(payload) = payload {
                            serde_json::from_str(payload[0].clone().as_str().unwrap()).unwrap()
                        } else {
                            return;
                        };
                        let now_playing: UserPlayingNowListen =
                            serde_json::from_value(payload).unwrap();

                        let track = TransformedTrack::from_listen(now_playing.track_metadata);

                        info!(
                            "new track: {} - {}",
                            track.name, track.artists[0].artist_credit_name
                        );

                        tokio::spawn(async move {
                            info!("enriching track...");
                            tx.send(match enrich_track(&client, &track).await {
                                Ok(track) => track,
                                Err(e) => {
                                    error!(
                                        "failed to enrich track {}: {}",
                                        track.name,
                                        e.to_string()
                                    );
                                    track
                                }
                            })
                            .expect("failed to broadcast new track");
                            info!("broadcasted enriched track")
                        });
                    }
                    .boxed()
                })
                .on("error", |err, socket| {
                    async move {
                        error!("Error: {:#?}", err);
                        socket.disconnect().await.unwrap()
                    }
                    .boxed()
                })
                .reconnect_on_disconnect(true);
            match socket_builder.connect().await {
                Ok(socket) => break socket,
                Err(e) => error!("{e}"),
            }
            info!("attempting reconnect in 5s...");
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
        }
    };
    info!("connected to listenbrainz socket");

    let socket = Arc::new(socket);
    HttpServer::new(move || {
        let socket = socket.clone();
        let logger = Logger::default();
        let rx = rx.clone();
        App::new()
            .wrap(logger)
            .wrap(Logger::new("%a %{User-Agent}i"))
            .service(web::resource("/ws").route(web::get().to(ws_handler)))
            .route("/", web::get().to(home))
            .app_data(web::Data::new(AppState {
                rx,
                _socketio: socket,
            }))
    })
    .bind(("0.0.0.0", 8080))?
    .run()
    .await?;
    Ok(())
}
