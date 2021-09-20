use super::*;
use crate::api::media_engine::{MIME_TYPE_H264, MIME_TYPE_OPUS, MIME_TYPE_VP8, MIME_TYPE_VP9};
use crate::api::setting_engine::SettingEngine;
use crate::api::APIBuilder;
use crate::media::rtp::rtp_codec::RTPCodecCapability;
use crate::media::rtp::rtp_receiver::RTPReceiver;
use crate::media::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use crate::media::track::track_remote::TrackRemote;
use crate::peer::peer_connection::peer_connection_test::{
    close_pair_now, create_vnet_pair, new_pair, send_video_until_done, signal_pair,
    until_connection_state,
};
use crate::peer::peer_connection_state::RTCPeerConnectionState;
use bytes::Bytes;
use std::sync::atomic::AtomicU64;
use tokio::time::Duration;
use waitgroup::WaitGroup;

#[tokio::test]
async fn test_rtp_sender_replace_track() -> Result<()> {
    let mut s = SettingEngine::default();
    s.disable_srtp_replay_protection(true);

    let mut m = MediaEngine::default();
    m.register_default_codecs()?;

    let api = APIBuilder::new()
        .with_setting_engine(s)
        .with_media_engine(m)
        .build();

    let (mut sender, mut receiver) = new_pair(&api).await?;

    let track_a = Arc::new(TrackLocalStaticSample::new(
        RTPCodecCapability {
            mime_type: MIME_TYPE_VP8.to_owned(),
            ..Default::default()
        },
        "video".to_owned(),
        "webrtc-rs".to_owned(),
    ));

    let track_b = Arc::new(TrackLocalStaticSample::new(
        RTPCodecCapability {
            mime_type: MIME_TYPE_H264.to_owned(),
            ..Default::default()
        },
        "video".to_owned(),
        "webrtc-rs".to_owned(),
    ));

    let rtp_sender = sender
        .add_track(Arc::clone(&track_a) as Arc<dyn TrackLocal + Send + Sync>)
        .await?;

    let (seen_packet_a_tx, seen_packet_a_rx) = mpsc::channel::<()>(1);
    let (seen_packet_b_tx, seen_packet_b_rx) = mpsc::channel::<()>(1);

    let seen_packet_a_tx = Arc::new(seen_packet_a_tx);
    let seen_packet_b_tx = Arc::new(seen_packet_b_tx);
    let on_track_count = Arc::new(AtomicU64::new(0));
    receiver
        .on_track(Box::new(
            move |track: Option<Arc<TrackRemote>>, _: Option<Arc<RTPReceiver>>| {
                assert_eq!(0, on_track_count.fetch_add(1, Ordering::SeqCst));
                let seen_packet_a_tx2 = Arc::clone(&seen_packet_a_tx);
                let seen_packet_b_tx2 = Arc::clone(&seen_packet_b_tx);
                Box::pin(async move {
                    while let Some(t) = &track {
                        let pkt = match t.read_rtp().await {
                            Ok((pkt, _)) => pkt,
                            Err(err) => {
                                //assert!(errors.Is(io.EOF, err))
                                log::debug!("{}", err);
                                return;
                            }
                        };

                        let last = pkt.payload[pkt.payload.len() - 1];
                        if last == 0xAA {
                            assert_eq!(t.codec().await.capability.mime_type, MIME_TYPE_VP8);
                            let _ = seen_packet_a_tx2.send(()).await;
                        } else if last == 0xBB {
                            assert_eq!(t.codec().await.capability.mime_type, MIME_TYPE_H264);
                            let _ = seen_packet_b_tx2.send(()).await;
                        } else {
                            assert!(false, "Unexpected RTP Data {:02x}", last);
                        }
                    }
                })
            },
        ))
        .await;

    signal_pair(&mut sender, &mut receiver).await?;

    // Block Until packet with 0xAA has been seen
    tokio::spawn(async move {
        send_video_until_done(seen_packet_a_rx, vec![track_a], Bytes::from_static(&[0xAA])).await;
    });

    rtp_sender
        .replace_track(Some(
            Arc::clone(&track_b) as Arc<dyn TrackLocal + Send + Sync>
        ))
        .await?;

    // Block Until packet with 0xBB has been seen
    tokio::spawn(async move {
        send_video_until_done(seen_packet_b_rx, vec![track_b], Bytes::from_static(&[0xBB])).await;
    });

    close_pair_now(&sender, &receiver).await;
    Ok(())
}

#[tokio::test]
async fn test_rtp_sender_get_parameters() -> Result<()> {
    let mut m = MediaEngine::default();
    m.register_default_codecs()?;
    let api = APIBuilder::new().with_media_engine(m).build();

    let (mut offerer, mut answerer) = new_pair(&api).await?;

    let rtp_transceiver = offerer
        .add_transceiver_from_kind(RTPCodecType::Video, &[])
        .await?;

    signal_pair(&mut offerer, &mut answerer).await?;

    if let Some(sender) = rtp_transceiver.sender().await {
        let parameters = sender.get_parameters().await;
        assert_ne!(0, parameters.rtp_parameters.codecs.len());
        assert_eq!(1, parameters.encodings.len());
        assert_eq!(sender.ssrc, parameters.encodings[0].ssrc);
    } else {
        assert!(false);
    }

    close_pair_now(&offerer, &answerer).await;
    Ok(())
}

#[tokio::test]
async fn test_rtp_sender_set_read_deadline() -> Result<()> {
    let (mut sender, mut receiver, wan) = create_vnet_pair().await?;

    let track = Arc::new(TrackLocalStaticSample::new(
        RTPCodecCapability {
            mime_type: MIME_TYPE_VP8.to_owned(),
            ..Default::default()
        },
        "video".to_owned(),
        "webrtc-rs".to_owned(),
    ));

    let rtp_sender = sender
        .add_track(Arc::clone(&track) as Arc<dyn TrackLocal + Send + Sync>)
        .await?;

    let peer_connections_connected = WaitGroup::new();
    until_connection_state(
        &mut sender,
        &peer_connections_connected,
        RTCPeerConnectionState::Connected,
    )
    .await;
    until_connection_state(
        &mut receiver,
        &peer_connections_connected,
        RTCPeerConnectionState::Connected,
    )
    .await;

    signal_pair(&mut sender, &mut receiver).await?;

    peer_connections_connected.wait().await;

    let result = tokio::time::timeout(Duration::from_secs(1), rtp_sender.read_rtcp()).await;
    assert!(result.is_err());

    {
        let mut w = wan.lock().await;
        w.stop().await?;
    }
    close_pair_now(&sender, &receiver).await;

    Ok(())
}

#[tokio::test]
async fn test_rtp_sender_replace_track_invalid_track_kind_change() -> Result<()> {
    let mut m = MediaEngine::default();
    m.register_default_codecs()?;
    let api = APIBuilder::new().with_media_engine(m).build();

    let (mut sender, mut receiver) = new_pair(&api).await?;

    let track_a = Arc::new(TrackLocalStaticSample::new(
        RTPCodecCapability {
            mime_type: MIME_TYPE_VP8.to_owned(),
            ..Default::default()
        },
        "video".to_owned(),
        "webrtc-rs".to_owned(),
    ));

    let track_b = Arc::new(TrackLocalStaticSample::new(
        RTPCodecCapability {
            mime_type: MIME_TYPE_OPUS.to_owned(),
            ..Default::default()
        },
        "audio".to_owned(),
        "webrtc-rs".to_owned(),
    ));

    let rtp_sender = sender
        .add_track(Arc::clone(&track_a) as Arc<dyn TrackLocal + Send + Sync>)
        .await?;

    signal_pair(&mut sender, &mut receiver).await?;

    let (seen_packet_tx, seen_packet_rx) = mpsc::channel::<()>(1);
    let seen_packet_tx = Arc::new(seen_packet_tx);
    receiver
        .on_track(Box::new(
            move |_: Option<Arc<TrackRemote>>, _: Option<Arc<RTPReceiver>>| {
                let seen_packet_tx2 = Arc::clone(&seen_packet_tx);
                Box::pin(async move {
                    let _ = seen_packet_tx2.send(()).await;
                })
            },
        ))
        .await;

    tokio::spawn(async move {
        send_video_until_done(seen_packet_rx, vec![track_a], Bytes::from_static(&[0xAA])).await;
    });

    if let Err(err) = rtp_sender.replace_track(Some(track_b)).await {
        assert!(Error::ErrRTPSenderNewTrackHasIncorrectKind.equal(&err));
    } else {
        assert!(false);
    }

    close_pair_now(&sender, &receiver).await;
    Ok(())
}

#[tokio::test]
async fn test_rtp_sender_replace_track_invalid_codec_change() -> Result<()> {
    let mut m = MediaEngine::default();
    m.register_default_codecs()?;
    let api = APIBuilder::new().with_media_engine(m).build();

    let (mut sender, mut receiver) = new_pair(&api).await?;

    let track_a = Arc::new(TrackLocalStaticSample::new(
        RTPCodecCapability {
            mime_type: MIME_TYPE_VP8.to_owned(),
            ..Default::default()
        },
        "video".to_owned(),
        "webrtc-rs".to_owned(),
    ));

    let track_b = Arc::new(TrackLocalStaticSample::new(
        RTPCodecCapability {
            mime_type: MIME_TYPE_VP9.to_owned(),
            ..Default::default()
        },
        "video".to_owned(),
        "webrtc-rs".to_owned(),
    ));

    let rtp_sender = sender
        .add_track(Arc::clone(&track_a) as Arc<dyn TrackLocal + Send + Sync>)
        .await?;

    {
        let tr = rtp_sender.tr.lock().await;
        if let Some(t) = &*tr {
            t.set_codec_preferences(vec![RTPCodecParameters {
                capability: RTPCodecCapability {
                    mime_type: MIME_TYPE_VP8.to_owned(),
                    ..Default::default()
                },
                payload_type: 96,
                ..Default::default()
            }])
            .await?;
        } else {
            assert!(false);
        }
    }

    signal_pair(&mut sender, &mut receiver).await?;

    let (seen_packet_tx, seen_packet_rx) = mpsc::channel::<()>(1);
    let seen_packet_tx = Arc::new(seen_packet_tx);
    receiver
        .on_track(Box::new(
            move |_: Option<Arc<TrackRemote>>, _: Option<Arc<RTPReceiver>>| {
                let seen_packet_tx2 = Arc::clone(&seen_packet_tx);
                Box::pin(async move {
                    let _ = seen_packet_tx2.send(()).await;
                })
            },
        ))
        .await;

    tokio::spawn(async move {
        send_video_until_done(seen_packet_rx, vec![track_a], Bytes::from_static(&[0xAA])).await;
    });

    if let Err(err) = rtp_sender.replace_track(Some(track_b)).await {
        assert!(Error::ErrUnsupportedCodec.equal(&err));
    } else {
        assert!(false);
    }

    close_pair_now(&sender, &receiver).await;
    Ok(())
}
