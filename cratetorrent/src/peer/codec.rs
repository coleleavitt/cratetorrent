use std::{
    convert::{TryFrom, TryInto},
    io::{self, Cursor},
};

use bytes::{Buf, BufMut, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

use crate::{Bitfield, BlockData, BlockInfo};

/// Handshake message exchanged once at connection start.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Handshake {
    pub prot: [u8; 19],
    pub reserved: [u8; 8],
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
}

impl Handshake {
    pub fn new(info_hash: [u8; 20], peer_id: [u8; 20]) -> Self {
        let mut prot = [0; 19];
        prot.copy_from_slice(PROTOCOL_STRING.as_bytes());
        Handshake { prot, reserved: [0; 8], info_hash, peer_id }
    }

    pub const fn len(&self) -> u64 {
        19 + 8 + 20 + 20
    }
}

pub(crate) const PROTOCOL_STRING: &str = "BitTorrent protocol";

/// Codec for the handshake.
pub(crate) struct HandshakeCodec;

impl Encoder<Handshake> for HandshakeCodec {
    type Error = io::Error;

    fn encode(
        &mut self,
        h: Handshake,
        buf: &mut BytesMut,
    ) -> io::Result<()> {
        buf.put_u8(h.prot.len() as u8);
        buf.extend_from_slice(&h.prot);
        buf.extend_from_slice(&h.reserved);
        buf.extend_from_slice(&h.info_hash);
        buf.extend_from_slice(&h.peer_id);
        Ok(())
    }
}

impl Decoder for HandshakeCodec {
    type Item = Handshake;
    type Error = io::Error;

    fn decode(
        &mut self,
        buf: &mut BytesMut,
    ) -> io::Result<Option<Handshake>> {
        if buf.is_empty() {
            return Ok(None);
        }

        // Peek protocol length
        let mut tmp = Cursor::new(&buf[..]);
        let prot_len = tmp.get_u8() as usize;
        if prot_len != PROTOCOL_STRING.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected protocol string length",
            ));
        }

        // Need full handshake: 1 + prot + 8 + 20 + 20
        let needed = 1 + prot_len + 8 + 20 + 20;
        if buf.len() < needed {
            return Ok(None);
        }

        // Consume length byte and read fields
        buf.advance(1);
        let mut prot = [0; 19];
        buf.copy_to_slice(&mut prot);
        let mut reserved = [0; 8];
        buf.copy_to_slice(&mut reserved);
        let mut info_hash = [0; 20];
        buf.copy_to_slice(&mut info_hash);
        let mut peer_id = [0; 20];
        buf.copy_to_slice(&mut peer_id);

        Ok(Some(Handshake {
            prot,
            reserved,
            info_hash,
            peer_id,
        }))
    }
}

/// IDs for peer‐wire messages (all but KeepAlive).
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum MessageId {
    Choke        = 0,
    Unchoke      = 1,
    Interested   = 2,
    NotInterested= 3,
    Have         = 4,
    Bitfield     = 5,
    Request      = 6,
    Block        = 7,
    Cancel       = 8,
}

impl TryFrom<u8> for MessageId {
    type Error = io::Error;

    fn try_from(v: u8) -> Result<Self, Self::Error> {
        use MessageId::*;
        match v {
            x if x == Choke as u8         => Ok(Choke),
            x if x == Unchoke as u8       => Ok(Unchoke),
            x if x == Interested as u8    => Ok(Interested),
            x if x == NotInterested as u8 => Ok(NotInterested),
            x if x == Have as u8          => Ok(Have),
            x if x == Bitfield as u8      => Ok(Bitfield),
            x if x == Request as u8       => Ok(Request),
            x if x == Block as u8         => Ok(Block),
            x if x == Cancel as u8        => Ok(Cancel),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unknown message ID",
            )),
        }
    }
}

impl MessageId {
    /// Total header length = 4-byte length prefix + 1-byte ID + optional fields.
    pub fn header_len(&self) -> u64 {
        let base = 4 + 1;
        match self {
            MessageId::Have    => base + 4,
            MessageId::Request => base + 3 * 4,
            MessageId::Block   => base + 2 * 4,
            MessageId::Cancel  => base + 3 * 4,
            _                  => base,
        }
    }
}

/// All peer‐wire messages (after handshake).
#[derive(Debug, PartialEq, Clone)]
pub(crate) enum Message {
    KeepAlive,
    Bitfield(Bitfield),
    Choke,
    Unchoke,
    Interested,
    NotInterested,
    Have { piece_index: usize },
    Request(BlockInfo),
    Block {
        piece_index: usize,
        offset: u32,
        data: BlockData,
    },
    Cancel(BlockInfo),
}

impl Message {
    /// For non‐KeepAlive messages, return the type ID.  KeepAlive has no ID.
    pub fn id(&self) -> Option<MessageId> {
        use Message::*;
        match self {
            KeepAlive      => None,
            Bitfield(_)    => Some(MessageId::Bitfield),
            Choke          => Some(MessageId::Choke),
            Unchoke        => Some(MessageId::Unchoke),
            Interested     => Some(MessageId::Interested),
            NotInterested  => Some(MessageId::NotInterested),
            Have { .. }    => Some(MessageId::Have),
            Request(_)     => Some(MessageId::Request),
            Block { .. }   => Some(MessageId::Block),
            Cancel(_)      => Some(MessageId::Cancel),
        }
    }

    /// Length of the protocol header (length‐prefix + ID + fixed fields).
    /// KeepAlive counts as 1 (the zero length field).
    pub fn protocol_len(&self) -> u64 {
        if let Some(id) = self.id() {
            id.header_len()
        } else {
            // KeepAlive: 4-byte prefix (0) is already counted in header_len
            // convention, so we return 1 to match original behavior.
            1
        }
    }
}

impl Encoder<Message> for PeerCodec {
    type Error = io::Error;

    fn encode(
        &mut self,
        msg: Message,
        buf: &mut BytesMut,
    ) -> io::Result<()> {
        use Message::*;
        match msg {
            KeepAlive => {
                buf.put_u32(0);
            }
            Bitfield(bf) => {
                let byte_len = bf.len() / 8;
                buf.put_u32((1 + byte_len) as u32);
                buf.put_u8(MessageId::Bitfield as u8);
                let bytes: Vec<u8> =
                    bf.as_raw_slice().iter().map(|&w| w as u8).collect();
                buf.extend_from_slice(&bytes);
            }
            Choke => {
                buf.put_u32(1);
                buf.put_u8(MessageId::Choke as u8);
            }
            Unchoke => {
                buf.put_u32(1);
                buf.put_u8(MessageId::Unchoke as u8);
            }
            Interested => {
                buf.put_u32(1);
                buf.put_u8(MessageId::Interested as u8);
            }
            NotInterested => {
                buf.put_u32(1);
                buf.put_u8(MessageId::NotInterested as u8);
            }
            Have { piece_index } => {
                buf.put_u32(1 + 4);
                buf.put_u8(MessageId::Have as u8);
                buf.put_u32(
                    piece_index
                        .try_into()
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?,
                );
            }
            Request(info) => {
                buf.put_u32(1 + 3 * 4);
                buf.put_u8(MessageId::Request as u8);
                info.encode(buf)?;
            }
            Block { piece_index, offset, data } => {
                let data_len = data.len() as u32;
                buf.put_u32(1 + 2 * 4 + data_len);
                buf.put_u8(MessageId::Block as u8);
                buf.put_u32(
                    piece_index
                        .try_into()
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?,
                );
                buf.put_u32(offset);
                buf.extend_from_slice(&data);
            }
            Cancel(info) => {
                buf.put_u32(1 + 3 * 4);
                buf.put_u8(MessageId::Cancel as u8);
                info.encode(buf)?;
            }
        }
        Ok(())
    }
}

impl Decoder for PeerCodec {
    type Item = Message;
    type Error = io::Error;

    fn decode(
        &mut self,
        buf: &mut BytesMut,
    ) -> io::Result<Option<Message>> {
        if buf.len() < 4 {
            return Ok(None);
        }
        let mut tmp = Cursor::new(&buf[..]);
        let msg_len = tmp.get_u32() as usize;
        if buf.len() < 4 + msg_len {
            return Ok(None);
        }
        buf.advance(4);

        if msg_len == 0 {
            return Ok(Some(Message::KeepAlive));
        }

        let id = MessageId::try_from(buf.get_u8())?;
        let msg = match id {
            MessageId::Choke => Message::Choke,
            MessageId::Unchoke => Message::Unchoke,
            MessageId::Interested => Message::Interested,
            MessageId::NotInterested => Message::NotInterested,
            MessageId::Have => {
                let idx = buf.get_u32();
                let pi = idx
                    .try_into()
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
                Message::Have { piece_index: pi }
            }
            MessageId::Bitfield => {
                let mut raw = vec![0u8; msg_len - 1];
                buf.copy_to_slice(&mut raw);
                let elems = raw.into_iter().map(|b| b as usize).collect();
                Message::Bitfield(Bitfield::from_vec(elems))
            }
            MessageId::Request => {
                let mut info = BlockInfo { piece_index: 0, offset: 0, len: 0 };
                info.piece_index = buf
                    .get_u32()
                    .try_into()
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
                info.offset = buf.get_u32();
                info.len    = buf.get_u32();
                Message::Request(info)
            }
            MessageId::Block => {
                let pi = buf.get_u32();
                let piece_index = pi
                    .try_into()
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
                let offset = buf.get_u32();
                let data_len = msg_len - 1 - 8;
                let mut data = vec![0u8; data_len];
                buf.copy_to_slice(&mut data);
                Message::Block {
                    piece_index,
                    offset,
                    data: data.into(),
                }
            }
            MessageId::Cancel => {
                let mut info = BlockInfo { piece_index: 0, offset: 0, len: 0 };
                info.piece_index = buf
                    .get_u32()
                    .try_into()
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
                info.offset = buf.get_u32();
                info.len    = buf.get_u32();
                Message::Cancel(info)
            }
        };

        Ok(Some(msg))
    }
}

/// Helper so that `info.encode(buf)?` works in both Request and Cancel arms.
impl BlockInfo {
    fn encode(&self, buf: &mut BytesMut) -> io::Result<()> {
        let idx: u32 = self
            .piece_index
            .try_into()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        buf.put_u32(idx);
        buf.put_u32(self.offset);
        buf.put_u32(self.len);
        Ok(())
    }
}

/// Codec for all peer‐wire messages after the handshake.
pub(crate) struct PeerCodec;
