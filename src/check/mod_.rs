use std::{collections::HashMap, convert::TryFrom};

use logos::Span;
use miette::Diagnostic;
use prost_types::{
    descriptor_proto::{ExtensionRange, ReservedRange},
    enum_descriptor_proto::EnumReservedRange,
    field_descriptor_proto, DescriptorProto, EnumDescriptorProto, EnumOptions,
    EnumValueDescriptorProto, ExtensionRangeOptions, FieldDescriptorProto, FieldOptions,
    FileDescriptorProto, FileOptions, MessageOptions, MethodDescriptorProto, MethodOptions,
    OneofDescriptorProto, OneofOptions, ServiceDescriptorProto, ServiceOptions, SourceCodeInfo,
};
use thiserror::Error;

use crate::{
    ast::{self, Visitor},
    case::{to_camel_case, to_pascal_case},
    check::names::NamePass,
    compile::ParsedFileMap,
    index_to_i32,
    lines::LineResolver,
    s, MAX_MESSAGE_FIELD_NUMBER,
};

pub(crate) use self::names::NameMap;

mod ir;
mod names;
mod span;
#[cfg(test)]
mod tests;

struct Context<'a> {
    syntax: ast::Syntax,
    errors: Vec<CheckError>,
    stack: Vec<Definition>,
    names: NameMap,
    file_map: Option<&'a ParsedFileMap>,
}

#[derive(Debug, Copy, Clone, PartialEq)]
enum DefinitionKind {
    Package,
    Message,
    Enum,
    EnumValue,
    Group,
    Oneof,
    Field,
    Service,
    Method,
}

impl ast::Message {
    fn to_message_descriptor(&self, ctx: &mut Context) -> DescriptorProto {
    }
}

impl ast::MessageBody {
    fn to_message_descriptor(&self, ctx: &mut Context) -> DescriptorProto {
        let mut field = Vec::new();
        let mut nested_type = Vec::new();
        let mut enum_type = Vec::new();
        let mut oneof_decl = Vec::new();
        let mut extension = Vec::new();

        for item in &self.items {
            match item {
                ast::MessageItem::Field(f) => {
                    f.to_field_descriptors(ctx, &mut nested_type, &mut field, &mut oneof_decl)
                }
                ast::MessageItem::Enum(e) => enum_type.push(e.to_enum_descriptor(ctx)),
                ast::MessageItem::Message(m) => nested_type.push(m.to_message_descriptor(ctx)),
                ast::MessageItem::Extend(e) => {
                    e.to_field_descriptors(ctx, &mut nested_type, &mut extension)
                }
            }
        }

        let mut extension_range = Vec::new();
        self.extensions
            .iter()
            .for_each(|e| e.to_extension_ranges(ctx, &mut extension_range));

        let options = if self.options.is_empty() {
            None
        } else {
            Some(ast::Option::to_message_options(&self.options))
        };

        let mut reserved_range = Vec::new();
        let mut reserved_name = Vec::new();
        for r in &self.reserved {
            match &r.kind {
                ast::ReservedKind::Ranges(ranges) => {
                    reserved_range.extend(ranges.iter().map(|r| r.to_reserved_range(ctx)))
                }
                ast::ReservedKind::Names(names) => {
                    reserved_name.extend(names.iter().map(|n| n.value.clone()))
                }
            }
        }

        DescriptorProto {
            name: None,
            field,
            extension,
            nested_type,
            enum_type,
            extension_range,
            oneof_decl,
            options,
            reserved_range,
            reserved_name,
        }
    }
}

impl ast::MessageField {
    fn to_field_descriptors(
        &self,
        ctx: &mut Context,
        messages: &mut Vec<DescriptorProto>,
        fields: &mut Vec<FieldDescriptorProto>,
        oneofs: &mut Vec<OneofDescriptorProto>,
    ) {
        if ctx.in_oneof()
            && matches!(
                self,
                ast::MessageField::Oneof(_) | ast::MessageField::Map(_)
            )
        {
            ctx.errors.push(CheckError::InvalidOneofFieldKind {
                kind: self.kind_name(),
                span: self.span(),
            });
            return;
        } else if ctx.in_extend()
            && matches!(
                self,
                ast::MessageField::Oneof(_) | ast::MessageField::Map(_)
            )
        {
            ctx.errors.push(CheckError::InvalidExtendFieldKind {
                kind: self.kind_name(),
                span: self.span(),
            });
            return;
        }

        match self {
            ast::MessageField::Field(field) => fields.push(field.to_field_descriptor(ctx)),
            ast::MessageField::Group(group) => {
                fields.push(group.to_field_descriptor(ctx, messages))
            }
            ast::MessageField::Map(map) => fields.push(map.to_field_descriptor(ctx, messages)),
            ast::MessageField::Oneof(oneof) => {
                oneofs.push(oneof.to_oneof_descriptor(ctx, messages, fields, oneofs.len()))
            }
        }
    }

    fn kind_name(&self) -> &'static str {
        match self {
            ast::MessageField::Field(_) => "normal",
            ast::MessageField::Group(_) => "group",
            ast::MessageField::Map(_) => "map",
            ast::MessageField::Oneof(_) => "oneof",
        }
    }

    fn span(&self) -> Span {
        match self {
            ast::MessageField::Field(field) => field.span.clone(),
            ast::MessageField::Group(field) => field.span.clone(),
            ast::MessageField::Map(field) => field.span.clone(),
            ast::MessageField::Oneof(field) => field.span.clone(),
        }
    }
}

impl ast::Field {
    fn to_field_descriptor(&self, ctx: &mut Context) -> FieldDescriptorProto {
        let name = s(&self.name.value);
        let number = self.number.to_field_number(ctx);
        let label = Some(
            self.label
                .unwrap_or(ast::FieldLabel::Optional)
                .to_field_label() as i32,
        );
        let (ty, type_name) = self.ty.to_type(ctx);

        let (default_value, options) = if self.options.is_empty() {
            (None, None)
        } else {
            let (default_value, options) = ast::OptionBody::to_field_options(&self.options);
            (default_value, Some(options))
        };

        ctx.check_label(self.label, self.span.clone());

        if default_value.is_some() && ty == Some(field_descriptor_proto::Type::Message) {
            ctx.errors.push(CheckError::InvalidDefault {
                kind: "message",
                span: self.span.clone(),
            })
        }

        let json_name = Some(to_camel_case(&self.name.value));

        let proto3_optional =
            if ctx.syntax == ast::Syntax::Proto3 && self.label == Some(ast::FieldLabel::Optional) {
                Some(true)
            } else {
                None
            };

        FieldDescriptorProto {
            name,
            number,
            label,
            r#type: ty.map(|t| t as i32),
            type_name,
            extendee: ctx.parent_extendee(),
            default_value,
            oneof_index: ctx.parent_oneof(),
            json_name,
            options,
            proto3_optional,
        }
    }
}

impl ast::Int {
    fn to_field_number(&self, ctx: &mut Context) -> Option<i32> {
        match (self.negative, i32::try_from(self.value)) {
            (false, Ok(number @ 1..=MAX_MESSAGE_FIELD_NUMBER)) => Some(number),
            _ => {
                ctx.errors.push(CheckError::InvalidMessageNumber {
                    span: self.span.clone(),
                });
                None
            }
        }
    }

    fn to_enum_number(&self, ctx: &mut Context) -> Option<i32> {
        let as_i32 = if self.negative {
            self.value.checked_neg().and_then(|n| i32::try_from(n).ok())
        } else {
            i32::try_from(self.value).ok()
        };

        if as_i32.is_none() {
            ctx.errors.push(CheckError::InvalidEnumNumber {
                span: self.span.clone(),
            });
        }

        as_i32
    }
}

impl ast::FieldLabel {
    fn to_field_label(self) -> field_descriptor_proto::Label {
        match self {
            ast::FieldLabel::Optional => field_descriptor_proto::Label::Optional,
            ast::FieldLabel::Required => field_descriptor_proto::Label::Required,
            ast::FieldLabel::Repeated => field_descriptor_proto::Label::Repeated,
        }
    }
}

impl ast::KeyTy {
    fn to_type(&self) -> field_descriptor_proto::Type {
        match self {
            ast::KeyTy::Int32 => field_descriptor_proto::Type::Int32,
            ast::KeyTy::Int64 => field_descriptor_proto::Type::Int64,
            ast::KeyTy::Uint32 => field_descriptor_proto::Type::Uint32,
            ast::KeyTy::Uint64 => field_descriptor_proto::Type::Uint64,
            ast::KeyTy::Sint32 => field_descriptor_proto::Type::Sint32,
            ast::KeyTy::Sint64 => field_descriptor_proto::Type::Sint64,
            ast::KeyTy::Fixed32 => field_descriptor_proto::Type::Fixed32,
            ast::KeyTy::Fixed64 => field_descriptor_proto::Type::Fixed64,
            ast::KeyTy::Sfixed32 => field_descriptor_proto::Type::Sfixed32,
            ast::KeyTy::Sfixed64 => field_descriptor_proto::Type::Sfixed64,
            ast::KeyTy::Bool => field_descriptor_proto::Type::Bool,
            ast::KeyTy::String => field_descriptor_proto::Type::String,
        }
    }
}

impl ast::Ty {
    fn to_type(&self, ctx: &mut Context) -> (Option<field_descriptor_proto::Type>, Option<String>) {
        match self {
            ast::Ty::Double => (Some(field_descriptor_proto::Type::Double), None),
            ast::Ty::Float => (Some(field_descriptor_proto::Type::Float), None),
            ast::Ty::Int32 => (Some(field_descriptor_proto::Type::Int32), None),
            ast::Ty::Int64 => (Some(field_descriptor_proto::Type::Int64), None),
            ast::Ty::Uint32 => (Some(field_descriptor_proto::Type::Uint32), None),
            ast::Ty::Uint64 => (Some(field_descriptor_proto::Type::Uint64), None),
            ast::Ty::Sint32 => (Some(field_descriptor_proto::Type::Sint32), None),
            ast::Ty::Sint64 => (Some(field_descriptor_proto::Type::Sint64), None),
            ast::Ty::Fixed32 => (Some(field_descriptor_proto::Type::Fixed32), None),
            ast::Ty::Fixed64 => (Some(field_descriptor_proto::Type::Fixed64), None),
            ast::Ty::Sfixed32 => (Some(field_descriptor_proto::Type::Sfixed32), None),
            ast::Ty::Sfixed64 => (Some(field_descriptor_proto::Type::Sfixed64), None),
            ast::Ty::Bool => (Some(field_descriptor_proto::Type::Bool), None),
            ast::Ty::String => (Some(field_descriptor_proto::Type::String), None),
            ast::Ty::Bytes => (Some(field_descriptor_proto::Type::Bytes), None),
            ast::Ty::Named(type_name) => match ctx.resolve_type_name(type_name) {
                (name, None) => (None, Some(name)),
                (name, Some(DefinitionKind::Message)) => {
                    (Some(field_descriptor_proto::Type::Message as _), Some(name))
                }
                (name, Some(DefinitionKind::Enum)) => {
                    (Some(field_descriptor_proto::Type::Enum as _), Some(name))
                }
                (name, Some(DefinitionKind::Group)) => {
                    (Some(field_descriptor_proto::Type::Group as _), Some(name))
                }
                (name, Some(_)) => {
                    ctx.errors.push(CheckError::InvalidMessageFieldTypeName {
                        name: type_name.to_string(),
                        span: type_name.span(),
                    });
                    (None, Some(name))
                }
            },
        }
    }
}

impl ast::Map {
    fn to_field_descriptor(
        &self,
        ctx: &mut Context,
        messages: &mut Vec<DescriptorProto>,
    ) -> FieldDescriptorProto {
        let name = s(&self.name.value);
        let number = self.number.to_field_number(ctx);

        let generated_message = self.generate_message_descriptor(ctx);
        let r#type = Some(field_descriptor_proto::Type::Message as i32);
        let (type_name, def) = ctx.resolve_relative_type_name(
            generated_message.name().to_owned(),
            self.name.span.clone(),
        );
        debug_assert_eq!(def, Some(DefinitionKind::Message));
        messages.push(generated_message);

        let (default_value, options) = if self.options.is_empty() {
            (None, None)
        } else {
            let (default_value, options) = ast::OptionBody::to_field_options(&self.options);
            (default_value, Some(options))
        };

        if self.label.is_some() {
            ctx.errors.push(CheckError::MapFieldWithLabel {
                span: self.span.clone(),
            });
        }

        if default_value.is_some() {
            ctx.errors.push(CheckError::InvalidDefault {
                kind: "map",
                span: self.span.clone(),
            });
        }

        let json_name = Some(to_camel_case(&self.name.value));

        FieldDescriptorProto {
            name,
            number,
            label: Some(field_descriptor_proto::Label::Repeated as _),
            r#type,
            type_name: Some(type_name),
            extendee: ctx.parent_extendee(),
            default_value: None,
            oneof_index: ctx.parent_oneof(),
            json_name,
            options,
            proto3_optional: None,
        }
    }

    fn generate_message_descriptor(&self, ctx: &mut Context) -> DescriptorProto {
        let name = Some(to_pascal_case(&self.name.value) + "Entry");

        let (ty, type_name) = self.ty.to_type(ctx);

        let key_field = FieldDescriptorProto {
            name: s("key"),
            number: Some(1),
            label: Some(field_descriptor_proto::Label::Optional as i32),
            r#type: Some(self.key_ty.to_type() as i32),
            json_name: s("key"),
            ..Default::default()
        };
        let value_field = FieldDescriptorProto {
            name: s("value"),
            number: Some(2),
            label: Some(field_descriptor_proto::Label::Optional as i32),
            r#type: ty.map(|t| t as i32),
            type_name,
            json_name: s("value"),
            ..Default::default()
        };

        DescriptorProto {
            name,
            field: vec![key_field, value_field],
            options: Some(MessageOptions {
                map_entry: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }
    }
}

impl ast::Group {
    fn to_field_descriptor(
        &self,
        ctx: &mut Context,
        messages: &mut Vec<DescriptorProto>,
    ) -> FieldDescriptorProto {
        let field_name = Some(self.name.value.to_ascii_lowercase());
        let message_name = Some(self.name.value.clone());

        let json_name = Some(to_camel_case(&self.name.value));
        let number = self.number.to_field_number(ctx);
        let label = Some(
            self.label
                .unwrap_or(ast::FieldLabel::Optional)
                .to_field_label() as i32,
        );

        let (default_value, options) = if self.options.is_empty() {
            (None, None)
        } else {
            let (default_value, options) = ast::OptionBody::to_field_options(&self.options);
            (default_value, Some(options))
        };

        if ctx.syntax == ast::Syntax::Proto3 {
            ctx.errors.push(CheckError::Proto3GroupField {
                span: self.span.clone(),
            });
        } else {
            ctx.check_label(self.label, self.span.clone());
        }

        if default_value.is_some() {
            ctx.errors.push(CheckError::InvalidDefault {
                kind: "group",
                span: self.span.clone(),
            });
        }

        ctx.enter(Definition::Group);

        let generated_message = DescriptorProto {
            name: message_name,
            ..self.body.to_message_descriptor(ctx)
        };
        ctx.exit();

        let r#type = Some(field_descriptor_proto::Type::Group as i32);
        let (type_name, def) = ctx.resolve_relative_type_name(
            generated_message.name().to_owned(),
            self.name.span.clone(),
        );
        debug_assert_eq!(def, Some(DefinitionKind::Group));
        messages.push(generated_message);

        FieldDescriptorProto {
            name: field_name,
            number,
            label,
            r#type,
            type_name: Some(type_name),
            extendee: ctx.parent_extendee(),
            default_value: None,
            oneof_index: ctx.parent_oneof(),
            json_name,
            options,
            proto3_optional: None,
        }
    }
}

impl ast::Extend {
    fn to_field_descriptors(
        &self,
        ctx: &mut Context,
        messages: &mut Vec<DescriptorProto>,
        fields: &mut Vec<FieldDescriptorProto>,
    ) {
        let (extendee, kind) = ctx.resolve_type_name(&self.extendee);
        if !matches!(
            kind,
            None | Some(DefinitionKind::Message) | Some(DefinitionKind::Group)
        ) {
            ctx.errors.push(CheckError::InvalidExtendeeTypeName {
                name: self.extendee.to_string(),
                span: self.extendee.span(),
            });
        }
        ctx.enter(Definition::Extend { extendee });

        for field in &self.fields {
            let mut oneofs = Vec::new();
            field.to_field_descriptors(ctx, messages, fields, &mut oneofs);
            debug_assert_eq!(oneofs, vec![]);
        }
        ctx.exit();
    }
}

impl ast::Oneof {
    fn to_oneof_descriptor(
        &self,
        ctx: &mut Context,
        messages: &mut Vec<DescriptorProto>,
        fields: &mut Vec<FieldDescriptorProto>,
        index: usize,
    ) -> OneofDescriptorProto {
        ctx.enter(Definition::Oneof {
            index: index_to_i32(index),
        });

        let name = s(&self.name.value);

        for field in &self.fields {
            let mut oneofs = Vec::new();
            field.to_field_descriptors(ctx, messages, fields, &mut oneofs);
            debug_assert_eq!(oneofs, vec![]);
        }

        let options = if self.options.is_empty() {
            None
        } else {
            Some(ast::Option::to_oneof_options(&self.options))
        };

        ctx.exit();
        OneofDescriptorProto { name, options }
    }
}

impl ast::Extensions {
    fn to_extension_ranges(&self, ctx: &mut Context, ranges: &mut Vec<ExtensionRange>) {
        let options = if self.options.is_empty() {
            None
        } else {
            Some(ast::OptionBody::to_extension_range_options(&self.options))
        };

        for range in &self.ranges {
            ranges.push(ExtensionRange {
                options: options.clone(),
                ..range.to_extension_range(ctx)
            });
        }
    }
}

impl ast::ReservedRange {
    fn to_reserved_range(&self, ctx: &mut Context) -> ReservedRange {
        let start = self.start.to_field_number(ctx);
        let end = match &self.end {
            ast::ReservedRangeEnd::None => start.map(|n| n + 1),
            ast::ReservedRangeEnd::Int(value) => value.to_field_number(ctx),
            ast::ReservedRangeEnd::Max => Some(MAX_MESSAGE_FIELD_NUMBER + 1),
        };

        ReservedRange { start, end }
    }

    fn to_extension_range(&self, ctx: &mut Context) -> ExtensionRange {
        let start = self.start.to_field_number(ctx);
        let end = match &self.end {
            ast::ReservedRangeEnd::None => start.map(|n| n + 1),
            ast::ReservedRangeEnd::Int(value) => value.to_field_number(ctx),
            ast::ReservedRangeEnd::Max => Some(MAX_MESSAGE_FIELD_NUMBER + 1),
        };

        ExtensionRange {
            start,
            end,
            ..Default::default()
        }
    }

    fn to_enum_reserved_range(&self, ctx: &mut Context) -> EnumReservedRange {
        let start = self.start.to_enum_number(ctx);
        let end = match &self.end {
            ast::ReservedRangeEnd::None => start,
            ast::ReservedRangeEnd::Int(value) => value.to_enum_number(ctx),
            ast::ReservedRangeEnd::Max => Some(i32::MAX),
        };

        EnumReservedRange { start, end }
    }
}

impl ast::Enum {
    fn to_enum_descriptor(&self, ctx: &mut Context) -> EnumDescriptorProto {
        ctx.enter(Definition::Enum);

        let name = s(&self.name.value);

        let value = self
            .values
            .iter()
            .map(|v| v.to_enum_value_descriptor(ctx))
            .collect();

        let options = if self.options.is_empty() {
            None
        } else {
            Some(ast::Option::to_enum_options(&self.options))
        };

        let mut reserved_range = Vec::new();
        let mut reserved_name = Vec::new();

        for r in &self.reserved {
            match &r.kind {
                ast::ReservedKind::Ranges(ranges) => {
                    reserved_range.extend(ranges.iter().map(|r| r.to_enum_reserved_range(ctx)))
                }
                ast::ReservedKind::Names(names) => {
                    reserved_name.extend(names.iter().map(|n| n.value.clone()))
                }
            }
        }

        ctx.exit();
        EnumDescriptorProto {
            name,
            value,
            options,
            reserved_range,
            reserved_name,
        }
    }
}

impl ast::EnumValue {
    fn to_enum_value_descriptor(&self, ctx: &mut Context) -> EnumValueDescriptorProto {
        let name = s(&self.name.value);

        let number = self.value.to_enum_number(ctx);

        let options = if self.options.is_empty() {
            None
        } else {
            Some(ast::OptionBody::to_enum_value_options(&self.options, ctx))
        };

        EnumValueDescriptorProto {
            name,
            number,
            options,
        }
    }
}

impl ast::Service {
    fn to_service_descriptor(&self, ctx: &mut Context) -> ServiceDescriptorProto {
        let name = s(&self.name);
        let options = if self.options.is_empty() {
            None
        } else {
            Some(ast::Option::to_service_options(&self.options))
        };

        ctx.enter(Definition::Service {
            full_name: ctx.full_name(&self.name.value),
        });

        let method = self
            .methods
            .iter()
            .map(|m| m.to_method_descriptor(ctx))
            .collect();

        ctx.exit();
        ServiceDescriptorProto {
            name,
            method,
            options,
        }
    }
}

impl ast::Method {
    fn to_method_descriptor(&self, ctx: &mut Context) -> MethodDescriptorProto {
        let name = s(&self.name);

        let (input_type, kind) = ctx.resolve_type_name(&self.input_ty);
        if !matches!(
            kind,
            None | Some(DefinitionKind::Message) | Some(DefinitionKind::Group)
        ) {
            ctx.errors.push(CheckError::InvalidMethodTypeName {
                name: self.input_ty.to_string(),
                kind: "input",
                span: self.input_ty.span(),
            })
        }

        let (output_type, kind) = ctx.resolve_type_name(&self.output_ty);
        if !matches!(
            kind,
            None | Some(DefinitionKind::Message) | Some(DefinitionKind::Group)
        ) {
            ctx.errors.push(CheckError::InvalidMethodTypeName {
                name: self.output_ty.to_string(),
                kind: "output",
                span: self.output_ty.span(),
            })
        }

        let options = if self.options.is_empty() {
            None
        } else {
            Some(ast::Option::to_method_options(&self.options))
        };

        let client_streaming = Some(self.is_client_streaming);
        let server_streaming = Some(self.is_server_streaming);

        MethodDescriptorProto {
            name,
            input_type: Some(input_type),
            output_type: Some(output_type),
            options,
            client_streaming,
            server_streaming,
        }
    }
}

impl ast::Option {
    fn to_file_options(_this: &[Self]) -> FileOptions {
        // todo!()
        Default::default()
    }

    fn to_message_options(_this: &[Self]) -> MessageOptions {
        todo!()
    }

    fn to_oneof_options(_this: &[Self]) -> OneofOptions {
        todo!()
    }

    fn to_enum_options(_this: &[Self]) -> EnumOptions {
        todo!()
    }

    fn to_service_options(_this: &[Self]) -> ServiceOptions {
        todo!()
    }

    fn to_method_options(_this: &[Self]) -> MethodOptions {
        todo!()
    }
}

impl ast::OptionBody {
    fn to_field_options(_this: &[Self]) -> (Option<String>, FieldOptions) {
        // todo!()
        Default::default()
    }

    fn to_extension_range_options(_this: &[Self]) -> ExtensionRangeOptions {
        todo!()
    }

    fn to_enum_value_options(_this: &[Self], _ctx: &mut Context) -> prost_types::EnumValueOptions {
        todo!()
    }
}

impl<'a> Context<'a> {
    fn add_name(&mut self, name: &str, kind: DefinitionKind, span: Span) {
        if let Err(err) = self.names.add(self.full_name(name), kind, span, None, true) {
            self.errors.push(err);
        }
    }

    fn enter(&mut self, def: Definition) {
        self.stack.push(def);
    }

    fn exit(&mut self) {
        self.stack.pop().expect("unbalanced stack");
    }

    fn resolve_type_name(&mut self, type_name: &ast::TypeName) -> (String, Option<DefinitionKind>) {
        let name = type_name.to_string();
        if self.file_map.is_none() {
            (name, None)
        } else if type_name.leading_dot.is_some() {
            if let Some(def) = self.names.get(&name) {
                (name, Some(def))
            } else {
                self.errors.push(CheckError::TypeNameNotFound {
                    name: name.clone(),
                    span: type_name.span(),
                });
                (name, None)
            }
        } else {
            self.resolve_relative_type_name(name, type_name.span())
        }
    }

    fn resolve_relative_type_name(
        &mut self,
        name: String,
        span: Span,
    ) -> (String, Option<DefinitionKind>) {
        for scope in self.stack.iter().rev() {
            let full_name = match scope {
                Definition::Message { full_name, .. }
                | Definition::Service { full_name, .. }
                | Definition::Package { full_name } => format!(".{}.{}", full_name, name),
                _ => continue,
            };

            if let Some(def) = self.names.get(&full_name) {
                return (full_name, Some(def));
            }
        }

        if let Some(def) = self.names.get(&name) {
            return (format!(".{}", name), Some(def));
        }

        self.errors.push(CheckError::TypeNameNotFound {
            name: name.to_owned(),
            span,
        });
        (name, None)
    }

    fn scope_name(&self) -> &str {
        for def in self.stack.iter().rev() {
            match def {
                Definition::Message { full_name, .. }
                | Definition::Service { full_name, .. }
                | Definition::Package { full_name } => return full_name.as_str(),
                _ => continue,
            }
        }

        ""
    }

    fn full_name(&self, name: &str) -> String {
        let namespace = self.scope_name();
        if namespace.is_empty() {
            name.to_owned()
        } else {
            format!("{}.{}", namespace, name)
        }
    }

    fn in_oneof(&self) -> bool {
        matches!(self.stack.last(), Some(Definition::Oneof { .. }))
    }

    fn in_extend(&self) -> bool {
        matches!(self.stack.last(), Some(Definition::Extend { .. }))
    }

    fn parent_extendee(&self) -> Option<String> {
        match self.stack.last() {
            Some(Definition::Extend { extendee, .. }) => Some(extendee.clone()),
            _ => None,
        }
    }

    fn parent_oneof(&self) -> Option<i32> {
        match self.stack.last() {
            Some(Definition::Oneof { index, .. }) => Some(*index),
            _ => None,
        }
    }

    fn check_label(&mut self, label: Option<ast::FieldLabel>, span: Span) {
        if self.in_extend() && label == Some(ast::FieldLabel::Required) {
            self.errors.push(CheckError::RequiredExtendField { span });
        } else if self.in_oneof() && label.is_some() {
            self.errors.push(CheckError::OneofFieldWithLabel { span });
        } else if self.syntax == ast::Syntax::Proto2 && label.is_none() && !self.in_oneof() {
            self.errors
                .push(CheckError::Proto2FieldMissingLabel { span });
        } else if self.syntax == ast::Syntax::Proto3 && label == Some(ast::FieldLabel::Required) {
            self.errors.push(CheckError::Proto3RequiredField { span });
        }
    }
}
