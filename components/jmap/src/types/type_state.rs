/*
 * Copyright (c) 2020-2022, Stalwart Labs Ltd.
 *
 * This file is part of the Stalwart JMAP Server.
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as
 * published by the Free Software Foundation, either version 3 of
 * the License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 * GNU Affero General Public License for more details.
 * in the LICENSE file at the top-level directory of this distribution.
 * You should have received a copy of the GNU Affero General Public License
 * along with this program.  If not, see <http://www.gnu.org/licenses/>.
 *
 * You can be released from the requirements of the AGPLv3 license by
 * purchasing a commercial license. Please contact licensing@stalw.art
 * for more details.
*/

use std::fmt::{self, Display};

use serde::{Deserialize, Serialize};
use store::core::{bitmap::BitmapItem, collection::Collection};

#[derive(Debug, Eq, PartialEq, Hash, Clone, Copy)]
#[repr(u8)]
pub enum TypeState {
    Email = 0,
    EmailDelivery = 1,
    EmailSubmission = 2,
    Mailbox = 3,
    Thread = 4,
    Identity = 5,
    None = 6,
}

impl From<u64> for TypeState {
    fn from(value: u64) -> Self {
        match value {
            0 => TypeState::Email,
            1 => TypeState::EmailDelivery,
            2 => TypeState::EmailSubmission,
            3 => TypeState::Mailbox,
            4 => TypeState::Thread,
            5 => TypeState::Identity,
            _ => {
                debug_assert!(false, "Invalid type_state value: {}", value);
                TypeState::None
            }
        }
    }
}

impl From<TypeState> for u64 {
    fn from(type_state: TypeState) -> u64 {
        type_state as u64
    }
}

impl BitmapItem for TypeState {
    fn max() -> u64 {
        TypeState::None as u64
    }

    fn is_valid(&self) -> bool {
        !matches!(self, TypeState::None)
    }
}

impl TryFrom<Collection> for TypeState {
    type Error = ();

    fn try_from(value: Collection) -> Result<Self, Self::Error> {
        match value {
            Collection::Mail => Ok(TypeState::Email),
            Collection::Mailbox => Ok(TypeState::Mailbox),
            Collection::Thread => Ok(TypeState::Thread),
            Collection::Identity => Ok(TypeState::Identity),
            Collection::EmailSubmission => Ok(TypeState::EmailSubmission),
            _ => Err(()),
        }
    }
}

impl TypeState {
    pub fn parse(value: &str) -> Self {
        match value {
            "Email" => TypeState::Email,
            "EmailDelivery" => TypeState::EmailDelivery,
            "EmailSubmission" => TypeState::EmailSubmission,
            "Mailbox" => TypeState::Mailbox,
            "Thread" => TypeState::Thread,
            "Identity" => TypeState::Identity,
            _ => TypeState::None,
        }
    }
}

impl Display for TypeState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TypeState::Email => write!(f, "Email"),
            TypeState::EmailDelivery => write!(f, "EmailDelivery"),
            TypeState::EmailSubmission => write!(f, "EmailSubmission"),
            TypeState::Mailbox => write!(f, "Mailbox"),
            TypeState::Thread => write!(f, "Thread"),
            TypeState::Identity => write!(f, "Identity"),
            TypeState::None => Ok(()),
        }
    }
}

// TypeState de/serialization
impl Serialize for TypeState {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}
struct TypeStateVisitor;

impl<'de> serde::de::Visitor<'de> for TypeStateVisitor {
    type Value = TypeState;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a valid JMAP TypeState")
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(TypeState::parse(v))
    }
}

impl<'de> Deserialize<'de> for TypeState {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_str(TypeStateVisitor)
    }
}
