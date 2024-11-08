// Copyright 2024 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use matrix_sdk_common::deserialized_responses::{
    UnableToDecryptInfo, UnableToDecryptReason, VerificationLevel,
};
use ruma::{events::AnySyncTimelineEvent, serde::Raw};
use serde::Deserialize;

/// Our best guess at the reason why an event can't be decrypted.
#[derive(Clone, Copy, Debug, Default, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum UtdCause {
    /// We don't have an explanation for why this UTD happened - it is probably
    /// a bug, or a network split between the two homeservers.
    #[default]
    Unknown = 0,

    /// We are missing the keys for this event, and the event was sent when we
    /// were not a member of the room (or invited).
    SentBeforeWeJoined = 1,

    /// The message was sent by a user identity we have not verified, but the
    /// user was previously verified.
    VerificationViolation = 2,

    /// The [`crate::TrustRequirement`] requires that the sending device be
    /// signed by its owner, and it was not.
    UnsignedDevice = 3,

    /// The [`crate::TrustRequirement`] requires that the sending device be
    /// signed by its owner, and we were unable to securely find the device.
    ///
    /// This could be because the device has since been deleted, because we
    /// haven't yet downloaded it from the server, or because the session
    /// data was obtained from an insecure source (imported from a file,
    /// obtained from a legacy (asymmetric) backup, unsafe key forward, etc.)
    UnknownDevice = 4,
}

/// MSC4115 membership info in the unsigned area.
#[derive(Deserialize)]
struct UnsignedWithMembership {
    #[serde(alias = "io.element.msc4115.membership")]
    membership: Membership,
}

/// MSC4115 contents of the membership property
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Membership {
    Leave,
    Invite,
    Join,
}

impl UtdCause {
    /// Decide the cause of this UTD, based on the evidence we have.
    pub fn determine(
        raw_event: Option<&Raw<AnySyncTimelineEvent>>,
        unable_to_decrypt_info: &UnableToDecryptInfo,
    ) -> Self {
        // TODO: in future, use more information to give a richer answer. E.g.
        match unable_to_decrypt_info.reason {
            UnableToDecryptReason::MissingMegolmSession
            | UnableToDecryptReason::UnknownMegolmMessageIndex => {
                // Look in the unsigned area for a `membership` field.
                if let Some(raw_event) = raw_event {
                    if let Ok(Some(unsigned)) =
                        raw_event.get_field::<UnsignedWithMembership>("unsigned")
                    {
                        if let Membership::Leave = unsigned.membership {
                            // We were not a member - this is the cause of the UTD
                            return UtdCause::SentBeforeWeJoined;
                        }
                    }
                }
                UtdCause::Unknown
            }

            UnableToDecryptReason::SenderIdentityNotTrusted(
                VerificationLevel::VerificationViolation,
            ) => UtdCause::VerificationViolation,

            UnableToDecryptReason::SenderIdentityNotTrusted(VerificationLevel::UnsignedDevice) => {
                UtdCause::UnsignedDevice
            }

            UnableToDecryptReason::SenderIdentityNotTrusted(VerificationLevel::None(_)) => {
                UtdCause::UnknownDevice
            }

            _ => UtdCause::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk_common::deserialized_responses::{
        DeviceLinkProblem, UnableToDecryptInfo, UnableToDecryptReason, VerificationLevel,
    };
    use ruma::{events::AnySyncTimelineEvent, serde::Raw};
    use serde_json::{json, value::to_raw_value};

    use crate::types::events::UtdCause;

    #[test]
    fn test_a_missing_raw_event_means_we_guess_unknown() {
        // When we don't provide any JSON to check for membership, then we guess the UTD
        // is unknown.
        assert_eq!(
            UtdCause::determine(
                None,
                &UnableToDecryptInfo {
                    session_id: None,
                    reason: UnableToDecryptReason::MissingMegolmSession,
                }
            ),
            UtdCause::Unknown
        );
    }

    #[test]
    fn test_if_there_is_no_membership_info_we_guess_unknown() {
        // If our JSON contains no membership info, then we guess the UTD is unknown.
        assert_eq!(
            UtdCause::determine(
                Some(&raw_event(json!({}))),
                &UnableToDecryptInfo {
                    session_id: None,
                    reason: UnableToDecryptReason::MissingMegolmSession
                }
            ),
            UtdCause::Unknown
        );
    }

    #[test]
    fn test_if_membership_info_cant_be_parsed_we_guess_unknown() {
        // If our JSON contains a membership property but not the JSON we expected, then
        // we guess the UTD is unknown.
        assert_eq!(
            UtdCause::determine(
                Some(&raw_event(json!({ "unsigned": { "membership": 3 } }))),
                &UnableToDecryptInfo {
                    session_id: None,
                    reason: UnableToDecryptReason::MissingMegolmSession
                }
            ),
            UtdCause::Unknown
        );
    }

    #[test]
    fn test_if_membership_is_invite_we_guess_unknown() {
        // If membership=invite then we expected to be sent the keys so the cause of the
        // UTD is unknown.
        assert_eq!(
            UtdCause::determine(
                Some(&raw_event(json!({ "unsigned": { "membership": "invite" } }),)),
                &UnableToDecryptInfo {
                    session_id: None,
                    reason: UnableToDecryptReason::MissingMegolmSession
                }
            ),
            UtdCause::Unknown
        );
    }

    #[test]
    fn test_if_membership_is_join_we_guess_unknown() {
        // If membership=join then we expected to be sent the keys so the cause of the
        // UTD is unknown.
        assert_eq!(
            UtdCause::determine(
                Some(&raw_event(json!({ "unsigned": { "membership": "join" } }))),
                &UnableToDecryptInfo {
                    session_id: None,
                    reason: UnableToDecryptReason::MissingMegolmSession
                }
            ),
            UtdCause::Unknown
        );
    }

    #[test]
    fn test_if_membership_is_leave_we_guess_membership() {
        // If membership=leave then we have an explanation for why we can't decrypt,
        // until we have MSC3061.
        assert_eq!(
            UtdCause::determine(
                Some(&raw_event(json!({ "unsigned": { "membership": "leave" } }))),
                &UnableToDecryptInfo {
                    session_id: None,
                    reason: UnableToDecryptReason::MissingMegolmSession
                }
            ),
            UtdCause::SentBeforeWeJoined
        );
    }

    #[test]
    fn test_if_reason_is_not_missing_key_we_guess_unknown_even_if_membership_is_leave() {
        // If the UnableToDecryptReason is other than MissingMegolmSession or
        // UnknownMegolmMessageIndex, we do not know the reason for the failure
        // even if membership=leave.
        assert_eq!(
            UtdCause::determine(
                Some(&raw_event(json!({ "unsigned": { "membership": "leave" } }))),
                &UnableToDecryptInfo {
                    session_id: None,
                    reason: UnableToDecryptReason::MalformedEncryptedEvent
                }
            ),
            UtdCause::Unknown
        );
    }

    #[test]
    fn test_if_unstable_prefix_membership_is_leave_we_guess_membership() {
        // Before MSC4115 is merged, we support the unstable prefix too.
        assert_eq!(
            UtdCause::determine(
                Some(&raw_event(
                    json!({ "unsigned": { "io.element.msc4115.membership": "leave" } })
                )),
                &UnableToDecryptInfo {
                    session_id: None,
                    reason: UnableToDecryptReason::MissingMegolmSession
                }
            ),
            UtdCause::SentBeforeWeJoined
        );
    }

    #[test]
    fn test_verification_violation_is_passed_through() {
        assert_eq!(
            UtdCause::determine(
                Some(&raw_event(json!({}))),
                &UnableToDecryptInfo {
                    session_id: None,
                    reason: UnableToDecryptReason::SenderIdentityNotTrusted(
                        VerificationLevel::VerificationViolation,
                    )
                }
            ),
            UtdCause::VerificationViolation
        );
    }

    #[test]
    fn test_unsigned_device_is_passed_through() {
        assert_eq!(
            UtdCause::determine(
                Some(&raw_event(json!({}))),
                &UnableToDecryptInfo {
                    session_id: None,
                    reason: UnableToDecryptReason::SenderIdentityNotTrusted(
                        VerificationLevel::UnsignedDevice,
                    )
                }
            ),
            UtdCause::UnsignedDevice
        );
    }

    #[test]
    fn test_unknown_device_is_passed_through() {
        assert_eq!(
            UtdCause::determine(
                Some(&raw_event(json!({}))),
                &UnableToDecryptInfo {
                    session_id: None,
                    reason: UnableToDecryptReason::SenderIdentityNotTrusted(
                        VerificationLevel::None(DeviceLinkProblem::MissingDevice)
                    )
                }
            ),
            UtdCause::UnknownDevice
        );
    }

    fn raw_event(value: serde_json::Value) -> Raw<AnySyncTimelineEvent> {
        Raw::from_json(to_raw_value(&value).unwrap())
    }
}
