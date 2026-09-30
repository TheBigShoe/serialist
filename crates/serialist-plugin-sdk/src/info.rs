//! Builders for the description types and constructors for errors.

use alloc::string::String;
use alloc::vec::Vec;

use crate::bindings::{
    BadField, CodecError, CodecInfo, CommandInfo, FieldInfo, FieldType, FrameKindInfo,
};

impl CodecInfo {
    /// A codec with no frame kinds and no commands yet.
    pub fn new(
        name: impl Into<String>,
        version: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            description: description.into(),
            kinds: Vec::new(),
            commands: Vec::new(),
        }
    }

    pub fn with_kind(mut self, kind: FrameKindInfo) -> Self {
        self.kinds.push(kind);
        self
    }

    /// Add a command. The first is the default for a saved command that names none.
    pub fn with_command(mut self, command: CommandInfo) -> Self {
        self.commands.push(command);
        self
    }
}

impl FrameKindInfo {
    pub fn new(kind: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            description: description.into(),
            fields: Vec::new(),
        }
    }

    pub fn with_field(mut self, field: FieldInfo) -> Self {
        self.fields.push(field);
        self
    }
}

impl CommandInfo {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            fields: Vec::new(),
        }
    }

    pub fn with_field(mut self, field: FieldInfo) -> Self {
        self.fields.push(field);
        self
    }
}

impl FieldInfo {
    /// A required field.
    pub fn new(name: impl Into<String>, ty: FieldType, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ty,
            description: description.into(),
            optional: false,
        }
    }

    /// A frame may leave it out; a command may be sent without it.
    pub fn optional(mut self) -> Self {
        self.optional = true;
        self
    }
}

impl CodecError {
    /// The codec has no command called `name`.
    pub fn unknown_command(name: impl Into<String>) -> Self {
        CodecError::UnknownCommand(name.into())
    }

    /// The required field `field` is absent.
    pub fn missing_field(field: impl Into<String>) -> Self {
        CodecError::MissingField(field.into())
    }

    /// `field` is present but unusable, for `reason`.
    pub fn bad_field(field: impl Into<String>, reason: impl Into<String>) -> Self {
        CodecError::BadField(BadField {
            field: field.into(),
            reason: reason.into(),
        })
    }

    /// Anything else.
    pub fn internal(message: impl Into<String>) -> Self {
        CodecError::Internal(message.into())
    }
}
