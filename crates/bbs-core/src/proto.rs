//! Types and gRPC stubs generated from `proto/bbs.proto`.
#![allow(dead_code)]

tonic::include_proto!("bbs.v2");

/// The one display form for an agent and its human: `name@owner`, or the bare
/// name when the owner is unknown. Humans are shown bare.
pub fn qualified_name(name: &str, owner: Option<&str>) -> String {
    match owner {
        Some(owner) => format!("{name}@{owner}"),
        None => name.to_string(),
    }
}

impl Message {
    /// Qualify a sender only when the receiving agent belongs to another human.
    pub fn sender_display_name(&self) -> String {
        let owner = match (&self.from_owner, &self.reader_owner) {
            (Some(owner), Some(reader))
                if self.from_kind() == Kind::Agent && !owner.eq_ignore_ascii_case(reader) =>
            {
                Some(owner.as_str())
            }
            _ => None,
        };
        qualified_name(&self.from_name, owner)
    }

    /// The sender for board-wide displays: agents always qualified, humans bare.
    pub fn qualified_sender(&self) -> String {
        match self.from_kind() {
            Kind::Human => self.from_name.clone(),
            _ => qualified_name(&self.from_name, self.from_owner.as_deref()),
        }
    }

    /// A group copy's sender as Hub shows it (D127): `ORIGINAL via GROUP`,
    /// with ` (reported)` when the original sender is in another namespace
    /// than the group, or `group GROUP (notice)` for a notice the group sent
    /// itself. None for other messages.
    pub fn group_copy_sender(&self) -> Option<String> {
        if self.group_notice == Some(true) {
            return Some(format!("group {} (notice)", self.from_name));
        }
        let via = self.via.as_deref()?;
        let reported = if self.reported == Some(true) {
            " (reported)"
        } else {
            ""
        };
        Some(format!("{} via {via}{reported}", self.from_name))
    }

    /// The direct recipient for board-wide displays, qualified like the sender.
    pub fn qualified_recipient(&self) -> Option<String> {
        let to = self.to_name.as_deref()?;
        Some(qualified_name(to, self.to_owner.as_deref()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message as _;

    #[test]
    fn group_copies_name_original_sender_and_group() {
        let mut msg = Message {
            from_kind: Kind::Remote.into(),
            from_name: "id:ann@hub".into(),
            via: Some("id:release@hub".into()),
            ..Default::default()
        };
        assert_eq!(
            msg.group_copy_sender().as_deref(),
            Some("id:ann@hub via id:release@hub")
        );
        msg.from_name = "id:ann@lab".into();
        msg.reported = Some(true);
        assert_eq!(
            msg.group_copy_sender().as_deref(),
            Some("id:ann@lab via id:release@hub (reported)")
        );
        let notice = Message {
            from_name: "id:release@hub".into(),
            group_notice: Some(true),
            ..Default::default()
        };
        assert_eq!(
            notice.group_copy_sender().as_deref(),
            Some("group id:release@hub (notice)")
        );
        msg.via = None;
        assert_eq!(msg.group_copy_sender(), None);
    }

    #[test]
    fn qualified_names_share_one_form() {
        assert_eq!(qualified_name("Ade", Some("will")), "Ade@will");
        assert_eq!(qualified_name("Ade", None), "Ade");
        let mut msg = Message {
            from_kind: Kind::Agent as i32,
            from_name: "Ade".to_string(),
            from_owner: Some("will".to_string()),
            reader_owner: Some("WILL".to_string()),
            to_name: Some("Bo".to_string()),
            to_owner: Some("ada".to_string()),
            ..Default::default()
        };
        // Inboxes qualify only senders owned by someone other than the reader.
        assert_eq!(msg.sender_display_name(), "Ade");
        assert_eq!(msg.qualified_sender(), "Ade@will");
        assert_eq!(msg.qualified_recipient().as_deref(), Some("Bo@ada"));
        msg.reader_owner = Some("ada".to_string());
        assert_eq!(msg.sender_display_name(), "Ade@will");
        msg.from_kind = Kind::Human as i32;
        msg.from_owner = None;
        assert_eq!(msg.qualified_sender(), "Ade");
        msg.to_name = None;
        assert_eq!(msg.qualified_recipient(), None);
    }

    #[test]
    fn message_round_trips() {
        let msg = Message {
            id: 7,
            created_at: "2026-10-12T23:14:33Z".to_string(),
            from_kind: Kind::Agent as i32,
            from_name: "Ade".to_string(),
            from_owner: Some("will".to_string()),
            reader_owner: Some("ada".to_string()),
            to_group: Some("bbs-core".to_string()),
            to_name: None,
            to_owner: None,
            read_at: None,
            body: "hello".to_string(),
            control: Some(Control::Cancel.into()),
            timer_id: Some(3),
            auto_reply: None,
            ..Default::default()
        };
        let decoded = Message::decode(msg.encode_to_vec().as_slice()).unwrap();
        assert_eq!(decoded, msg);
    }
}
