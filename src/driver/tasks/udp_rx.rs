use super::{
    error::{Error, Result},
    message::*,
    Config,
};
use crate::{
    constants::*,
    driver::DecodeMode,
    events::{internal_data::*, CoreContext},
};
use audiopus::{
    coder::Decoder as OpusDecoder,
    error::{Error as OpusError, ErrorCode},
    packet::Packet as OpusPacket,
    Channels,
};
use crypto_secretbox::XSalsa20Poly1305 as Cipher;
use discortp::{
    demux::{self, DemuxedMut},
    rtp::{RtpExtensionPacket, RtpPacket},
    FromPacket,
    Packet,
    PacketSize,
};
use flume::Receiver;
use std::{collections::HashMap, convert::TryInto, sync::Arc};
use tokio::{net::UdpSocket, select};
use tracing::{error, instrument, trace, warn};

#[derive(Debug)]
struct SsrcState {
    silent_frame_count: u16,
    decoder: OpusDecoder,
    last_seq: u16,
    decode_size: PacketDecodeSize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PacketDecodeSize {
    /// Minimum frame size on Discord.
    TwentyMillis,
    /// Hybrid packet, sent by Firefox web client.
    ///
    /// Likely 20ms frame + 10ms frame.
    ThirtyMillis,
    /// Next largest frame size.
    FortyMillis,
    /// Maximum Opus frame size.
    SixtyMillis,
    /// Maximum Opus packet size: 120ms.
    Max,
}

impl PacketDecodeSize {
    fn bump_up(self) -> Self {
        use PacketDecodeSize::*;
        match self {
            TwentyMillis => ThirtyMillis,
            ThirtyMillis => FortyMillis,
            FortyMillis => SixtyMillis,
            SixtyMillis | Max => Max,
        }
    }

    fn can_bump_up(self) -> bool {
        self != PacketDecodeSize::Max
    }

    fn len(self) -> usize {
        use PacketDecodeSize::*;
        match self {
            TwentyMillis => STEREO_FRAME_SIZE,
            ThirtyMillis => (STEREO_FRAME_SIZE / 2) * 3,
            FortyMillis => 2 * STEREO_FRAME_SIZE,
            SixtyMillis => 3 * STEREO_FRAME_SIZE,
            Max => 6 * STEREO_FRAME_SIZE,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SpeakingDelta {
    Same,
    Start,
    Stop,
}

impl SsrcState {
    fn new(pkt: RtpPacket<'_>) -> Self {
        Self {
            silent_frame_count: 5, // We do this to make the first speech packet fire an event.
            decoder: OpusDecoder::new(SAMPLE_RATE, Channels::Stereo)
                .expect("Failed to create new Opus decoder for source."),
            last_seq: pkt.get_sequence().into(),
            decode_size: PacketDecodeSize::TwentyMillis,
        }
    }

    fn process(
        &mut self,
        pkt: RtpPacket<'_>,
        data_offset: usize,
        data_trailer: usize,
        decode_mode: DecodeMode,
        decrypted: bool,
    ) -> Result<(SpeakingDelta, Option<Vec<i16>>)> {
        let new_seq: u16 = pkt.get_sequence().into();
        let payload_len = pkt.payload().len();

        let extensions = pkt.get_extension() != 0;
        let seq_delta = new_seq.wrapping_sub(self.last_seq);
        Ok(if seq_delta >= (1 << 15) {
            // Overflow, reordered (previously missing) packet.
            (SpeakingDelta::Same, Some(vec![]))
        } else {
            self.last_seq = new_seq;
            let missed_packets = seq_delta.saturating_sub(1);

            // Note: we still need to handle this for non-decoded.
            // This is mainly because packet events and speaking events can be handed to the
            // user.
            let (audio, pkt_size) = if decode_mode.should_decrypt() && decrypted {
                self.scan_and_decode(
                    &pkt.payload()[data_offset..payload_len - data_trailer],
                    extensions,
                    missed_packets,
                    decode_mode == DecodeMode::Decode,
                )?
            } else {
                // The latter part is an upper bound, as we cannot determine
                // how long packet extensions are.
                // WIthout decryption, speaking detection is thus broken.
                (None, payload_len - data_offset - data_trailer)
            };

            let delta = if pkt_size == SILENT_FRAME.len() {
                // Frame is silent.
                let old = self.silent_frame_count;
                self.silent_frame_count =
                    self.silent_frame_count.saturating_add(1 + missed_packets);

                if self.silent_frame_count >= 5 && old < 5 {
                    SpeakingDelta::Stop
                } else {
                    SpeakingDelta::Same
                }
            } else {
                // Frame has meaningful audio.
                let out = if self.silent_frame_count >= 5 {
                    SpeakingDelta::Start
                } else {
                    SpeakingDelta::Same
                };
                self.silent_frame_count = 0;
                out
            };

            (delta, audio)
        })
    }

    fn scan_and_decode(
        &mut self,
        data: &[u8],
        extension: bool,
        missed_packets: u16,
        decode: bool,
    ) -> Result<(Option<Vec<i16>>, usize)> {
        let start = if extension {
            RtpExtensionPacket::new(data)
                .map(|pkt| pkt.packet_size())
                .ok_or_else(|| {
                    error!("Extension packet indicated, but insufficient space.");
                    Error::IllegalVoicePacket
                })
        } else {
            Ok(0)
        }?;

        let pkt = if decode {
            let mut out = vec![0; self.decode_size.len()];

            for _ in 0..missed_packets {
                let missing_frame: Option<OpusPacket> = None;
                let dest_samples = (&mut out[..])
                    .try_into()
                    .expect("Decode logic will cap decode buffer size at i32::MAX.");
                if let Err(e) = self.decoder.decode(missing_frame, dest_samples, false) {
                    warn!("Issue while decoding for missed packet: {:?}.", e);
                }
            }

            // In general, we should expect 20 ms frames.
            // However, Discord occasionally like to surprise us with something bigger.
            // This is *sender-dependent behaviour*.
            //
            // This should scan up to find the "correct" size that a source is using,
            // and then remember that.
            loop {
                let tried_audio_len = self.decoder.decode(
                    Some((&data[start..]).try_into()?),
                    (&mut out[..]).try_into()?,
                    false,
                );

                match tried_audio_len {
                    Ok(audio_len) => {
                        // Decoding to stereo: audio_len refers to sample count irrespective of channel count.
                        // => multiply by number of channels.
                        out.truncate(2 * audio_len);

                        break;
                    },
                    Err(OpusError::Opus(ErrorCode::BufferTooSmall)) => {
                        if self.decode_size.can_bump_up() {
                            self.decode_size = self.decode_size.bump_up();
                            out = vec![0; self.decode_size.len()];
                        } else {
                            error!("Received packet larger than Opus standard maximum,");
                            return Err(Error::IllegalVoicePacket);
                        }
                    },
                    Err(e) => {
                        error!("Failed to decode received packet: {:?}.", e);
                        return Err(e.into());
                    },
                }
            }

            Some(out)
        } else {
            None
        };

        Ok((pkt, data.len() - start))
    }
}

struct UdpRx {
    cipher: Cipher,
    decoder_map: HashMap<u32, SsrcState>,
    #[allow(dead_code)]
    config: Config,
    packet_buffer: [u8; VOICE_PACKET_MAX],
    rx: Receiver<UdpRxMessage>,

    udp_socket: Arc<UdpSocket>,
}

impl UdpRx {
    #[instrument(skip(self))]
    async fn run(&mut self, interconnect: &mut Interconnect) {
        loop {
            select! {
                Ok((len, _addr)) = self.udp_socket.recv_from(&mut self.packet_buffer[..]) => {
                    self.process_udp_message(interconnect, len);
                }
                msg = self.rx.recv_async() => {
                    use UdpRxMessage::*;
                    match msg {
                        Ok(ReplaceInterconnect(i)) => {
                            *interconnect = i;
                        },
                        Ok(SetConfig(c)) => {
                            self.config = c;
                        },
                        Ok(Poison) | Err(_) => break,
                    }
                }
            }
        }
    }

    fn process_udp_message(&mut self, interconnect: &Interconnect, len: usize) {
        // NOTE: errors here (and in general for UDP) are not fatal to the connection.
        // Panics should be avoided due to adversarial nature of rx'd packets,
        // but correct handling should not prompt a reconnect.
        //
        // For simplicity, we nominate the mixing context to rebuild the event
        // context if it fails (hence, the `let _ =` statements.), as it will try to
        // make contact every 20ms.
        let crypto_mode = self.config.crypto_mode;
        let packet = &mut self.packet_buffer[..len];

        match demux::demux_mut(packet) {
            DemuxedMut::Rtp(mut rtp) => {
                if !rtp_valid(rtp.to_immutable()) {
                    error!("Illegal RTP message received.");
                    return;
                }

                let packet_data = if self.config.decode_mode.should_decrypt() {
                    let out = crypto_mode
                        .decrypt_in_place(&mut rtp, &self.cipher)
                        .map(|(s, t)| (s, t, true));

                    if let Err(e) = out {
                        warn!("RTP decryption failed: {:?}", e);
                    }

                    out.ok()
                } else {
                    None
                };

                let (rtp_body_start, rtp_body_tail, decrypted) = packet_data.unwrap_or_else(|| {
                    (
                        crypto_mode.payload_prefix_len(),
                        crypto_mode.payload_suffix_len(),
                        false,
                    )
                });

                let entry = self
                    .decoder_map
                    .entry(rtp.get_ssrc())
                    .or_insert_with(|| SsrcState::new(rtp.to_immutable()));

                if let Ok((delta, audio)) = entry.process(
                    rtp.to_immutable(),
                    rtp_body_start,
                    rtp_body_tail,
                    self.config.decode_mode,
                    decrypted,
                ) {
                    match delta {
                        SpeakingDelta::Start => {
                            let _ = interconnect.events.send(EventMessage::FireCoreEvent(
                                CoreContext::SpeakingUpdate(InternalSpeakingUpdate {
                                    ssrc: rtp.get_ssrc(),
                                    speaking: true,
                                }),
                            ));
                        },
                        SpeakingDelta::Stop => {
                            let _ = interconnect.events.send(EventMessage::FireCoreEvent(
                                CoreContext::SpeakingUpdate(InternalSpeakingUpdate {
                                    ssrc: rtp.get_ssrc(),
                                    speaking: false,
                                }),
                            ));
                        },
                        _ => {},
                    }

                    let _ = interconnect.events.send(EventMessage::FireCoreEvent(
                        CoreContext::VoicePacket(InternalVoicePacket {
                            audio,
                            packet: rtp.from_packet(),
                            payload_offset: rtp_body_start,
                            payload_end_pad: rtp_body_tail,
                        }),
                    ));
                } else {
                    warn!("RTP decoding/processing failed.");
                }
            },
            DemuxedMut::Rtcp(mut rtcp) => {
                let packet_data = if self.config.decode_mode.should_decrypt() {
                    let out = crypto_mode.decrypt_in_place(&mut rtcp, &self.cipher);

                    if let Err(e) = out {
                        warn!("RTCP decryption failed: {:?}", e);
                    }

                    out.ok()
                } else {
                    None
                };

                let (start, tail) = packet_data.unwrap_or_else(|| {
                    (
                        crypto_mode.payload_prefix_len(),
                        crypto_mode.payload_suffix_len(),
                    )
                });

                let _ =
                    interconnect
                        .events
                        .send(EventMessage::FireCoreEvent(CoreContext::RtcpPacket(
                            InternalRtcpPacket {
                                packet: rtcp.from_packet(),
                                payload_offset: start,
                                payload_end_pad: tail,
                            },
                        )));
            },
            DemuxedMut::FailedParse(t) => {
                warn!("Failed to parse message of type {:?}.", t);
            },
            _ => {
                warn!("Illegal UDP packet from voice server.");
            },
        }
    }
}

#[instrument(skip(interconnect, rx, cipher))]
pub(crate) async fn runner(
    mut interconnect: Interconnect,
    rx: Receiver<UdpRxMessage>,
    cipher: Cipher,
    config: Config,
    udp_socket: Arc<UdpSocket>,
) {
    trace!("UDP receive handle started.");

    let mut state = UdpRx {
        cipher,
        decoder_map: Default::default(),
        config,
        packet_buffer: [0u8; VOICE_PACKET_MAX],
        rx,
        udp_socket,
    };

    state.run(&mut interconnect).await;

    trace!("UDP receive handle stopped.");
}

#[inline]
fn rtp_valid(packet: RtpPacket<'_>) -> bool {
    packet.get_version() == RTP_VERSION && packet.get_payload_type() == RTP_PROFILE_TYPE
}
