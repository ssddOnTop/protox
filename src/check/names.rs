use std::{
    borrow::Cow,
    collections::{hash_map, HashMap},
    fmt,
    iter::once,
    mem,
};

use logos::Span;
use miette::{Diagnostic, LabeledSpan};
use once_cell::sync::Lazy;

use crate::{
    compile::{ParsedFile, ParsedFileMap},
    file::GoogleFileResolver,
    index_to_i32, make_absolute_name, make_name, parse_namespace,
    types::{
        field_descriptor_proto, DescriptorProto, EnumDescriptorProto, FieldDescriptorProto,
        FileDescriptorProto, OneofDescriptorProto, ServiceDescriptorProto,
    },
    Compiler,
};

use super::CheckError;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DuplicateNameError {
    pub name: String,
    pub first: NameLocation,
    pub second: NameLocation,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum NameLocation {
    Import(String),
    Root(Span),
    Unknown,
}

/// A simple map of all definitions in a proto file for checking downstream files.
#[derive(Debug, Default)]
pub(crate) struct NameMap {
    map: HashMap<String, Entry>,
}

#[derive(Debug, Clone)]
struct Entry {
    kind: DefinitionKind,
    span: Option<Span>,
    public: bool,
    file: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DefinitionKind {
    Package,
    Message,
    Enum,
    EnumValue {
        number: i32,
    },
    Oneof,
    Field {
        number: i32,
        ty: Option<field_descriptor_proto::Type>,
        type_name: Option<String>,
        label: Option<field_descriptor_proto::Label>,
        oneof_index: Option<i32>,
        extendee: Option<String>,
    },
    Service,
    Method,
}

struct NamePass {
    name_map: NameMap,
    scope: String,
    path: Vec<i32>,
    errors: Vec<CheckError>,
}

impl NameMap {
    pub fn from_proto(
        file: &FileDescriptorProto,
        file_map: &ParsedFileMap,
    ) -> Result<NameMap, Vec<CheckError>> {
        let mut ctx = NamePass {
            name_map: NameMap::new(),
            errors: Vec::new(),
            path: Vec::new(),
            scope: String::new(),
        };

        ctx.add_file_descriptor_proto(file, file_map);
        debug_assert!(ctx.scope.is_empty());

        if ctx.errors.is_empty() {
            Ok(ctx.name_map)
        } else {
            Err(ctx.errors)
        }
    }

    pub fn google_descriptor() -> &'static Self {
        static INSTANCE: Lazy<NameMap> = Lazy::new(|| {
            let mut compiler = Compiler::with_file_resolver(GoogleFileResolver::new());
            compiler
                .add_file("google/protobuf/descriptor.proto")
                .expect("invalid descriptor.proto");
            let mut file_map = compiler.into_parsed_file_map();
            mem::take(&mut file_map["google/protobuf/descriptor.proto"].name_map)
        });

        &INSTANCE
    }

    fn new() -> Self {
        NameMap::default()
    }

    fn add(
        &mut self,
        name: String,
        kind: DefinitionKind,
        span: Option<Span>,
        file: Option<&str>,
        public: bool,
    ) -> Result<(), DuplicateNameError> {
        match self.map.entry(name) {
            hash_map::Entry::Vacant(entry) => {
                entry.insert(Entry {
                    file: file.map(ToOwned::to_owned),
                    kind,
                    span,
                    public,
                });
                Ok(())
            }
            hash_map::Entry::Occupied(entry) => match (kind, &entry.get().kind) {
                (DefinitionKind::Package, DefinitionKind::Package) => Ok(()),
                _ => {
                    let first =
                        NameLocation::new(entry.get().file.clone(), entry.get().span.clone());
                    let second = NameLocation::new(file.map(ToOwned::to_owned), span);

                    Err(DuplicateNameError {
                        name: entry.key().to_owned(),
                        first,
                        second,
                    })
                }
            },
        }
    }

    fn merge(&mut self, other: &Self, file: String, public: bool) -> Result<(), CheckError> {
        for (name, entry) in &other.map {
            if entry.public {
                self.add(
                    name.clone(),
                    entry.kind.clone(),
                    entry.span.clone(), // todo None?
                    Some(&file),
                    public,
                )?;
            }
        }
        Ok(())
    }

    pub(super) fn get(&self, name: &str) -> Option<&DefinitionKind> {
        self.map.get(name).map(|e| &e.kind)
    }

    pub(super) fn resolve<'a>(
        &self,
        context: &str,
        name: &'a str,
    ) -> Option<(Cow<'a, str>, &DefinitionKind)> {
        if let Some(absolute_name) = name.strip_prefix('.') {
            self.get(absolute_name)
                .map(|def| (Cow::Borrowed(name), def))
        } else {
            let mut context = context;

            loop {
                let full_name = make_absolute_name(context, name);
                if let Some(def) = self.get(&full_name[1..]) {
                    return Some((Cow::Owned(full_name), def));
                }

                if context.is_empty() {
                    return None;
                }
                context = parse_namespace(context);
            }
        }
    }
}

impl NamePass {
    fn add_name<'a>(
        &mut self,
        name: impl Into<Cow<'a, str>>,
        kind: DefinitionKind,
        span: Option<Span>,
    ) {
        if let Err(err) = self
            .name_map
            .add(self.full_name(name), kind, span, None, true)
        {
            self.errors.push(CheckError::DuplicateName(err));
        }
    }

    fn merge_names(&mut self, file: &ParsedFile, public: bool) {
        if let Err(err) = self
            .name_map
            .merge(&file.name_map, file.name().to_owned(), public)
        {
            self.errors.push(err);
        }
    }

    fn full_name<'a>(&self, name: impl Into<Cow<'a, str>>) -> String {
        make_name(&self.scope, name.into())
    }

    fn enter(&mut self, name: &str) {
        if !self.scope.is_empty() {
            self.scope.push('.');
        }
        self.scope.push_str(name);
    }

    fn exit(&mut self) {
        debug_assert!(!self.scope.is_empty(), "imbalanced scope stack");
        let len = self.scope.rfind('.').unwrap_or(0);
        self.scope.truncate(len);
    }

    fn add_file_descriptor_proto(&mut self, file: &FileDescriptorProto, file_map: &ParsedFileMap) {
        for (index, import) in file.dependency.iter().enumerate() {
            let import_file = &file_map[import.as_str()];
            self.merge_names(
                import_file,
                file.public_dependency.contains(&index_to_i32(index)),
            );
        }

        for part in file.package().split('.') {
            self.add_name(part, DefinitionKind::Package, None);
            self.enter(part);
        }

        for message in &file.message_type {
            self.add_descriptor_proto(message);
        }

        for enu in &file.enum_type {
            self.add_enum_descriptor_proto(enu);
        }

        for extend in &file.extension {
            self.add_field_descriptor_proto(extend);
        }

        for service in &file.service {
            self.add_service_descriptor_proto(service);
        }

        for _ in file.package().split('.') {
            self.exit();
        }
    }

    fn add_descriptor_proto(&mut self, message: &DescriptorProto) {
        self.add_name(message.name(), DefinitionKind::Message, None);
        self.enter(message.name());

        for field in &message.field {
            self.add_field_descriptor_proto(field)
        }

        for oneof in &message.oneof_decl {
            self.add_oneof_descriptor_proto(oneof);
        }

        for message in &message.nested_type {
            self.add_descriptor_proto(message);
        }

        for enu in &message.enum_type {
            self.add_enum_descriptor_proto(enu);
        }

        for extension in &message.extension {
            self.add_field_descriptor_proto(extension);
        }

        self.exit();
    }

    fn add_field_descriptor_proto(&mut self, field: &FieldDescriptorProto) {
        self.add_name(
            field.name(),
            DefinitionKind::Field {
                ty: field_descriptor_proto::Type::from_i32(field.r#type.unwrap_or(0)),
                type_name: field.type_name.clone(),
                number: field.number(),
                label: field_descriptor_proto::Label::from_i32(field.label.unwrap_or(0)),
                oneof_index: field.oneof_index,
                extendee: field.extendee.clone(),
            },
            None,
        );
    }

    fn add_oneof_descriptor_proto(&mut self, oneof: &OneofDescriptorProto) {
        self.add_name(oneof.name(), DefinitionKind::Oneof, None);
    }

    fn add_enum_descriptor_proto(&mut self, enu: &EnumDescriptorProto) {
        self.add_name(enu.name(), DefinitionKind::Enum, None);

        for value in &enu.value {
            self.add_name(
                value.name(),
                DefinitionKind::EnumValue {
                    number: value.number(),
                },
                None,
            );
        }
    }

    fn add_service_descriptor_proto(&mut self, service: &ServiceDescriptorProto) {
        self.add_name(service.name(), DefinitionKind::Service, None);

        self.enter(service.name());
        for method in &service.method {
            self.add_name(method.name(), DefinitionKind::Method, None);
        }
        self.exit();
    }
}

impl NameLocation {
    fn new(file: Option<String>, span: Option<Span>) -> NameLocation {
        match (file, span) {
            (Some(file), _) => NameLocation::Import(file),
            (None, Some(span)) => NameLocation::Root(span),
            (None, None) => NameLocation::Unknown,
        }
    }
}

impl fmt::Display for DuplicateNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.first, &self.second) {
            (NameLocation::Import(first), NameLocation::Import(second)) => write!(
                f,
                "name '{}' is defined both in imported file '{}' and '{}'",
                self.name, first, second
            ),
            (NameLocation::Import(first), NameLocation::Root(_) | NameLocation::Unknown) => write!(
                f,
                "name '{}' is already defined in imported file '{}'",
                self.name, first
            ),
            _ => write!(f, "name '{}' is defined twice", self.name),
        }
    }
}

impl std::error::Error for DuplicateNameError {}

impl Diagnostic for DuplicateNameError {
    fn labels(&self) -> Option<Box<dyn Iterator<Item = LabeledSpan> + '_>> {
        match (&self.first, &self.second) {
            (NameLocation::Root(first), NameLocation::Root(second)) => Some(Box::new(
                vec![
                    LabeledSpan::new_with_span(
                        Some("first defined here…".to_owned()),
                        first.clone(),
                    ),
                    LabeledSpan::new_with_span(
                        Some("…and defined again here".to_owned()),
                        second.clone(),
                    ),
                ]
                .into_iter(),
            )),
            (_, NameLocation::Root(span)) | (NameLocation::Root(span), _) => Some(Box::new(once(
                LabeledSpan::new_with_span(Some("defined here".to_owned()), span.clone()),
            ))),
            _ => None,
        }
    }
}
