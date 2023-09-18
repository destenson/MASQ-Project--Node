// Copyright (c) 2019, MASQ (https://masq.ai) and/or its affiliates. All rights reserved.

use serde::de::Visitor;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;
use std::fmt;
use uuid::Uuid;

#[derive(Hash, PartialEq, Eq, Clone, Copy)]
pub struct StreamKey {
    pub hash: HashType,
}

impl fmt::Debug for StreamKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        let string = base64::encode_config(&self.hash, base64::STANDARD_NO_PAD);
        write!(f, "{}", string)
    }
}

impl fmt::Display for StreamKey {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        let debug: &dyn fmt::Debug = self;
        debug.fmt(f)
    }
}

impl Serialize for StreamKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bytes(&self.hash[..])
    }
}

impl<'de> Deserialize<'de> for StreamKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_bytes(StreamKeyVisitor)
    }
}

struct StreamKeyVisitor;

impl<'a> Visitor<'a> for StreamKeyVisitor {
    type Value = StreamKey;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a StreamKey struct")
    }

    fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        if v.len() != sha1::DIGEST_LENGTH {
            return Err(serde::de::Error::custom(format!(
                "can't deserialize bytes from {:?}",
                v
            )));
        }

        let mut x: HashType = [0; sha1::DIGEST_LENGTH];
        x.copy_from_slice(v); // :(

        Ok(StreamKey { hash: x })
    }
}

impl StreamKey {
    pub fn new() -> StreamKey {
        let mut hash = sha1::Sha1::new();
        let uuid = Uuid::new_v4();
        // eprintln!("This is how UUID looks: {}", uuid);
        let uuid_bytes: &[u8] = uuid.as_bytes();
        hash.update(uuid_bytes);
        // match peer_addr.ip() {
        //     IpAddr::V4(ipv4) => hash.update(&ipv4.octets()),
        //     IpAddr::V6(_ipv6) => unimplemented!(),
        // }
        // hash.update(&[
        //     (peer_addr.port() >> 8) as u8,
        //     (peer_addr.port() & 0xFF) as u8,
        // ]);
        // hash.update(public_key.as_slice());
        StreamKey {
            hash: hash.digest().bytes(),
        }
    }
}

type HashType = [u8; sha1::DIGEST_LENGTH];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn stream_keys_are_unique() {
        let mut stream_keys_set = HashSet::new();

        for i in 1..=1000 {
            let stream_key = StreamKey::new();
            let is_unique = stream_keys_set.insert(stream_key);

            assert!(is_unique, "{}", &format!("Stream key {i} is not unique"));
        }
    }

    #[test]
    fn debug_implementation() {
        let subject = StreamKey::new();

        let result = format!("{:?}", subject);

        assert_eq!(result, subject.to_string());
    }

    #[test]
    fn display_implementation() {
        let subject = StreamKey::new();

        let result = format!("{}", subject);

        assert_eq!(result, subject.to_string());
    }

    #[test]
    fn serialization_and_deserialization_can_talk() {
        let subject = StreamKey::new();

        let serial = serde_cbor::ser::to_vec(&subject).unwrap();

        let result = serde_cbor::de::from_slice::<StreamKey>(serial.as_slice()).unwrap();

        assert_eq!(result, subject);
    }
}
