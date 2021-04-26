// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Generic primitives related to code signing.

use {
    crate::{
        code_directory::{CodeSignatureFlags, ExecutableSegmentFlags},
        code_requirement::CodeRequirementExpression,
        error::AppleCodesignError,
        macho::{Blob, DigestType, RequirementBlob},
    },
    cryptographic_message_syntax::{Certificate, SigningKey},
    goblin::mach::cputype::{
        CpuType, CPU_TYPE_ARM, CPU_TYPE_ARM64, CPU_TYPE_ARM64_32, CPU_TYPE_X86_64,
    },
    reqwest::{IntoUrl, Url},
    std::{collections::BTreeMap, convert::TryFrom, fmt::Formatter},
};

/// Denotes the scope for a setting.
///
/// Settings have an associated scope defined by this type. This allows settings
/// to apply to exactly what you want them to apply to.
///
/// Scopes can be converted from a string representation. The following syntax is
/// recognized:
///
/// * `@main` - Maps to [SettingsScope::Main]
/// * `@<int>` - e.g. `@0`. Maps to [SettingsScope::MultiArchIndex].Index
/// * `@[cpu_type=<int>]` - e.g. `@[cpu_type=7]`. Maps to [SettingsScope::MultiArchCpuType].
/// * `@[cpu_type=<string>]` - e.g. `@[cpu_type=x86_64]`. Maps to [SettingsScope::MultiArchCpuType]
///    for recognized string values (see below).
/// * `<string>` - e.g. `path/to/file`. Maps to [SettingsScope::Path].
/// * `<string>@<int>` - e.g. `path/to/file@0`. Maps to [SettingsScope::PathMultiArchIndex].
/// * `<string>@[cpu_type=<int>]` - e.g. `path/to/file@[cpu_type=7]`. Maps to
///   [SettingsScope::PathMultiArchCpuType].
/// * `<string>@[cpu_type=<string>]` - e.g. `path/to/file@[cpu_type=arm64]`. Maps to
///   [SettingsScope::PathMultiArchCpuType] for recognized string values (see below).
///
/// # Recognized cpu_type String Values
///
/// The following `cpu_type=` string values are recognized:
///
/// * `arm` -> [CPU_TYPE_ARM]
/// * `arm64` -> [CPU_TYPE_ARM64]
/// * `arm64_32` -> [CPU_TYPE_ARM64_32]
/// * `x86_64` -> [CPU_TYPE_X86_64]
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SettingsScope {
    // The order of the variants is important. Instance cloning iterates keys in
    // sorted order and last write wins. So the order here should be from widest to
    // most granular.
    /// The main entity being signed.
    ///
    /// Can be a Mach-O file, a bundle, or any other primitive this crate
    /// supports signing.
    ///
    /// When signing a bundle or any primitive with nested elements (such as a
    /// fat/universal Mach-O binary), settings can propagate to nested elements.
    Main,

    /// Filesystem path.
    ///
    /// Can refer to a Mach-O file, a nested bundle, or any other filesystem
    /// based primitive that can be traversed into when performing nested signing.
    ///
    /// The string value refers to the filesystem relative path of the entity
    /// relative to the main entity being signed.
    Path(String),

    /// A single Mach-O binary within a fat/universal Mach-O binary.
    ///
    /// The binary to operate on is defined by its 0-based index within the
    /// fat/universal Mach-O container.
    MultiArchIndex(usize),

    /// A single Mach-O binary within a fat/universal Mach-O binary.
    ///
    /// The binary to operate on is defined by its CPU architecture.
    MultiArchCpuType(CpuType),

    /// Combination of [SettingsScope::Path] and [SettingsScope::MultiArchIndex].
    ///
    /// This refers to a single Mach-O binary within a fat/universal binary at a
    /// given relative path.
    PathMultiArchIndex(String, usize),

    /// Combination of [SettingsScope::Path] and [SettingsScope::MultiArchCpuType].
    ///
    /// This refers to a single Mach-O binary within a fat/universal binary at a
    /// given relative path.
    PathMultiArchCpuType(String, CpuType),
}

impl std::fmt::Display for SettingsScope {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Main => f.write_str("main signing target"),
            Self::Path(path) => f.write_fmt(format_args!("path {}", path)),
            Self::MultiArchIndex(index) => f.write_fmt(format_args!(
                "fat/universal Mach-O binaries at index {}",
                index
            )),
            Self::MultiArchCpuType(cpu_type) => f.write_fmt(format_args!(
                "fat/universal Mach-O binaries for CPU {}",
                cpu_type
            )),
            Self::PathMultiArchIndex(path, index) => f.write_fmt(format_args!(
                "fat/universal Mach-O binaries at index {} under path {}",
                index, path
            )),
            Self::PathMultiArchCpuType(path, cpu_type) => f.write_fmt(format_args!(
                "fat/universal Mach-O binaries for CPU {} under path {}",
                cpu_type, path
            )),
        }
    }
}

impl SettingsScope {
    fn parse_at_expr(
        at_expr: &str,
    ) -> Result<(Option<usize>, Option<CpuType>), AppleCodesignError> {
        match at_expr.parse::<usize>() {
            Ok(index) => Ok((Some(index), None)),
            Err(_) => {
                if at_expr.starts_with('[') && at_expr.ends_with(']') {
                    let v = &at_expr[1..at_expr.len() - 1];
                    let parts = v.split('=').collect::<Vec<_>>();

                    if parts.len() == 2 {
                        let (key, value) = (parts[0], parts[1]);

                        if key != "cpu_type" {
                            return Err(AppleCodesignError::ParseSettingsScope(format!(
                                "in '@{}', {} not recognized; must be cpu_type",
                                at_expr, key
                            )));
                        }

                        if let Some(cpu_type) = match value {
                            "arm" => Some(CPU_TYPE_ARM),
                            "arm64" => Some(CPU_TYPE_ARM64),
                            "arm64_32" => Some(CPU_TYPE_ARM64_32),
                            "x86_64" => Some(CPU_TYPE_X86_64),
                            _ => None,
                        } {
                            return Ok((None, Some(cpu_type)));
                        }

                        match value.parse::<u32>() {
                            Ok(cpu_type) => Ok((None, Some(cpu_type as CpuType))),
                            Err(_) => Err(AppleCodesignError::ParseSettingsScope(format!(
                                "in '@{}', cpu_arch value {} not recognized",
                                at_expr, value
                            ))),
                        }
                    } else {
                        Err(AppleCodesignError::ParseSettingsScope(format!(
                            "'{}' sub-expression isn't of form <key>=<value>",
                            v
                        )))
                    }
                } else {
                    Err(AppleCodesignError::ParseSettingsScope(format!(
                        "in '{}', @ expression not recognized",
                        at_expr
                    )))
                }
            }
        }
    }
}

impl AsRef<SettingsScope> for SettingsScope {
    fn as_ref(&self) -> &SettingsScope {
        self
    }
}

impl TryFrom<&str> for SettingsScope {
    type Error = AppleCodesignError;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        if s == "@main" {
            Ok(Self::Main)
        } else if let Some(at_expr) = s.strip_prefix('@') {
            match Self::parse_at_expr(at_expr)? {
                (Some(index), None) => Ok(Self::MultiArchIndex(index)),
                (None, Some(cpu_type)) => Ok(Self::MultiArchCpuType(cpu_type)),
                _ => panic!("this shouldn't happen"),
            }
        } else {
            // Looks like a path.
            let parts = s.rsplitn(2, '@').collect::<Vec<_>>();

            match parts.len() {
                1 => Ok(Self::Path(s.to_string())),
                2 => {
                    // Parts are reversed since splitting at end.
                    let (at_expr, path) = (parts[0], parts[1]);

                    match Self::parse_at_expr(at_expr)? {
                        (Some(index), None) => {
                            Ok(Self::PathMultiArchIndex(path.to_string(), index))
                        }
                        (None, Some(cpu_type)) => {
                            Ok(Self::PathMultiArchCpuType(path.to_string(), cpu_type))
                        }
                        _ => panic!("this shouldn't happen"),
                    }
                }
                _ => panic!("this shouldn't happen"),
            }
        }
    }
}

/// Represents code signing settings.
///
/// This type holds settings related to a single logical signing operation.
/// Some settings (such as the signing key-pair are global). Other settings
/// (such as the entitlements or designated requirement) can be applied on a
/// more granular, scoped basis. The scoping of these lower-level settings is
/// controlled via [SettingsScope]. If a setting is specified with a scope, it
/// only applies to that scope. See that type's documentation for more.
///
/// An instance of this type is bound to a signing operation. When the
/// signing operation traverses into nested primitives (e.g. when traversing
/// into the individual Mach-O binaries in a fat/universal binary or when
/// traversing into nested bundles or non-main binaries within a bundle), a
/// new instance of this type is transparently constructed by merging global
/// settings with settings for the target scope. This allows granular control
/// over which signing settings apply to which entity and enables a signing
/// operation over a complex primitive to be configured/performed via a single
/// [SigningSettings] and signing operation.
#[derive(Clone, Debug, Default)]
pub struct SigningSettings<'key> {
    // Global settings.
    signing_key: Option<(&'key SigningKey, Certificate)>,
    certificates: Vec<Certificate>,
    time_stamp_url: Option<Url>,
    team_name: Option<String>,
    digest_type: DigestType,

    // Scope-specific settings.
    // These are BTreeMap so when we filter the keys, keys with higher precedence come
    // last and last write wins.
    identifiers: BTreeMap<SettingsScope, String>,
    entitlements: BTreeMap<SettingsScope, String>,
    designated_requirement: BTreeMap<SettingsScope, Vec<Vec<u8>>>,
    code_signature_flags: BTreeMap<SettingsScope, CodeSignatureFlags>,
    executable_segment_flags: BTreeMap<SettingsScope, ExecutableSegmentFlags>,
    info_plist_data: BTreeMap<SettingsScope, Vec<u8>>,
    code_resources_data: BTreeMap<SettingsScope, Vec<u8>>,
}

impl<'key> SigningSettings<'key> {
    /// Obtain the digest type to use.
    pub fn digest_type(&self) -> &DigestType {
        &self.digest_type
    }

    /// Set the content digest to use.
    ///
    /// The default is SHA-256. Changing this to SHA-1 can weaken security of digital
    /// signatures and may prevent the binary from running in environments that enforce
    /// more modern signatures.
    pub fn set_digest_type(&mut self, digest_type: DigestType) {
        self.digest_type = digest_type;
    }

    /// Obtain the signing key to use.
    pub fn signing_key(&self) -> Option<&(&'key SigningKey, Certificate)> {
        self.signing_key.as_ref()
    }

    /// Set the signing key-pair for producing a cryptographic signature over code.
    ///
    /// If this is not called, signing will lack a cryptographic signature and will only
    /// contain digests of content. This is known as "ad-hoc" mode. Binaries lacking a
    /// cryptographic signature or signed without a key-pair issued/signed by Apple may
    /// not run in all environments.
    pub fn set_signing_key(&mut self, private: &'key SigningKey, public: Certificate) {
        self.signing_key = Some((private, public));
    }

    /// Obtain the certificate chain.
    pub fn certificate_chain(&self) -> &[Certificate] {
        &self.certificates
    }

    /// Add a parsed certificate to the signing certificate chain.
    ///
    /// When producing a cryptographic signature (see [SigningSettings::set_signing_key]),
    /// information about the signing key-pair is included in the signature. The signing
    /// key's public certificate is always included. This function can be used to define
    /// additional X.509 public certificates to include. Typically, the signing chain
    /// of the signing key-pair up until the root Certificate Authority (CA) is added
    /// so clients have access to the full certificate chain for validation purposes.
    ///
    /// This setting has no effect if [SigningSettings::set_signing_key] is not called.
    pub fn chain_certificate(&mut self, cert: Certificate) {
        self.certificates.push(cert);
    }

    /// Add a DER encoded X.509 public certificate to the signing certificate chain.
    ///
    /// This is like [Self::chain_certificate] except the certificate data is provided in
    /// its binary, DER encoded form.
    pub fn chain_certificate_der(
        &mut self,
        data: impl AsRef<[u8]>,
    ) -> Result<(), AppleCodesignError> {
        self.chain_certificate(Certificate::from_der(data.as_ref())?);

        Ok(())
    }

    /// Add a PEM encoded X.509 public certificate to the signing certificate chain.
    ///
    /// This is like [Self::chain_certificate] except the certificate is
    /// specified as PEM encoded data. This is a human readable string like
    /// `-----BEGIN CERTIFICATE-----` and is a common method for encoding certificate data.
    /// (PEM is effectively base64 encoded DER data.)
    ///
    /// Only a single certificate is read from the PEM data.
    pub fn chain_certificate_pem(
        &mut self,
        data: impl AsRef<[u8]>,
    ) -> Result<(), AppleCodesignError> {
        self.chain_certificate(Certificate::from_pem(data.as_ref())?);

        Ok(())
    }

    /// Obtain the Time-Stamp Protocol server URL.
    pub fn time_stamp_url(&self) -> Option<&Url> {
        self.time_stamp_url.as_ref()
    }

    /// Set the Time-Stamp Protocol server URL to use to generate a Time-Stamp Token.
    ///
    /// When set and a signing key-pair is defined, the server will be contacted during
    /// signing and a Time-Stamp Token will be embedded in the cryptographic signature.
    /// This Time-Stamp Token is a cryptographic proof that someone in possession of
    /// the signing key-pair produced the cryptographic signature at a given time. It
    /// facilitates validation of the signing time via an independent (presumably trusted)
    /// entity.
    pub fn set_time_stamp_url(&mut self, url: impl IntoUrl) -> Result<(), AppleCodesignError> {
        self.time_stamp_url = Some(url.into_url()?);

        Ok(())
    }

    /// Obtain the team name/identifier for signed binaries.
    pub fn team_name(&self) -> Option<&str> {
        self.team_name.as_deref()
    }

    /// Set the team name/identifier for signed binaries.
    pub fn set_team_name(&mut self, value: impl ToString) {
        self.team_name = Some(value.to_string());
    }

    /// Obtain the binary identifier string for a given scope.
    pub fn binary_identifier(&self, scope: impl AsRef<SettingsScope>) -> Option<&str> {
        self.identifiers.get(scope.as_ref()).map(|s| s.as_str())
    }

    /// Set the binary identifier string for a binary at a path.
    ///
    /// This only has an effect when signing an individual Mach-O file (use the `None` path)
    /// or the non-main executable in a bundle: when signing the main executable in a bundle,
    /// the binary's identifier is retrieved from the mandatory `CFBundleIdentifier` value in
    /// the bundle's `Info.plist` file.
    ///
    /// The binary identifier should be a DNS-like name and should uniquely identify the
    /// binary. e.g. `com.example.my_program`
    pub fn set_binary_identifier(&mut self, scope: SettingsScope, value: impl ToString) {
        self.identifiers.insert(scope, value.to_string());
    }

    /// Obtain the entitlements XML string for a given scope.
    pub fn entitlements_xml(&self, scope: impl AsRef<SettingsScope>) -> Option<&str> {
        self.entitlements.get(scope.as_ref()).map(|s| s.as_str())
    }

    /// Set the entitlements to sign via an XML string.
    ///
    /// The value should be an XML plist. The value is not validated.
    pub fn set_entitlements_xml(&mut self, scope: SettingsScope, value: impl ToString) {
        self.entitlements.insert(scope, value.to_string());
    }

    /// Obtain the designated requirements binary expressions for a given scope.
    pub fn designated_requirement(
        &self,
        scope: impl AsRef<SettingsScope>,
    ) -> Option<&Vec<Vec<u8>>> {
        self.designated_requirement.get(scope.as_ref())
    }

    /// Set the designated requirement for a Mach-O binary given a [CodeRequirementExpression].
    ///
    /// The designated requirement (also known as "code requirements") specifies run-time
    /// requirements for the binary. e.g. you can stipulate that the binary must be
    /// signed by a certificate issued/signed/chained to Apple. The designated requirement
    /// is embedded in Mach-O binaries and signed.
    pub fn set_designated_requirement_expression(
        &mut self,
        scope: SettingsScope,
        expr: &CodeRequirementExpression,
    ) -> Result<(), AppleCodesignError> {
        self.designated_requirement
            .insert(scope, vec![expr.to_bytes()?]);

        Ok(())
    }

    /// Set the designated requirement expression for a Mach-O binary given serialized bytes.
    ///
    /// This is like [SigningSettings::designated_requirement_expression] except the
    /// designated requirement expression is given as serialized bytes. The bytes passed are
    /// the value that would be produced by compiling a code requirement expression via
    /// `csreq -b`.
    pub fn set_designated_requirement_bytes(
        &mut self,
        scope: SettingsScope,
        data: impl AsRef<[u8]>,
    ) -> Result<(), AppleCodesignError> {
        let blob = RequirementBlob::from_blob_bytes(data.as_ref())?;

        self.designated_requirement.insert(
            scope,
            blob.parse_expressions()?
                .iter()
                .map(|x| x.to_bytes())
                .collect::<Result<Vec<_>, AppleCodesignError>>()?,
        );

        Ok(())
    }

    /// Obtain the code signature flags for a given scope.
    pub fn code_signature_flags(
        &self,
        scope: impl AsRef<SettingsScope>,
    ) -> Option<CodeSignatureFlags> {
        self.code_signature_flags.get(scope.as_ref()).copied()
    }

    /// Set code signature flags for signed Mach-O binaries.
    ///
    /// The incoming flags will replace any already-defined flags.
    pub fn set_code_signature_flags(&mut self, scope: SettingsScope, flags: CodeSignatureFlags) {
        self.code_signature_flags.insert(scope, flags);
    }

    /// Add code signature flags for signed Mach-O binaries.
    ///
    /// The incoming flags will be ORd with any existing flags for the path
    /// specified. The new flags will be returned.
    pub fn add_code_signature_flags(
        &mut self,
        scope: SettingsScope,
        flags: CodeSignatureFlags,
    ) -> CodeSignatureFlags {
        let existing = self
            .code_signature_flags
            .get(&scope)
            .copied()
            .unwrap_or_else(CodeSignatureFlags::empty);

        let new = existing | flags;

        self.code_signature_flags.insert(scope, new);

        new
    }

    /// Remove code signature flags for signed Mach-O binaries.
    ///
    /// The incoming flags will be removed from any existing flags for the path
    /// specified. The new flags will be returned.
    pub fn remove_code_signature_flags(
        &mut self,
        scope: SettingsScope,
        flags: CodeSignatureFlags,
    ) -> CodeSignatureFlags {
        let existing = self
            .code_signature_flags
            .get(&scope)
            .copied()
            .unwrap_or_else(CodeSignatureFlags::empty);

        let new = existing - flags;

        self.code_signature_flags.insert(scope, new);

        new
    }

    /// Obtain the executable segment flags for a given scope.
    pub fn executable_segment_flags(
        &self,
        scope: impl AsRef<SettingsScope>,
    ) -> Option<ExecutableSegmentFlags> {
        self.executable_segment_flags.get(scope.as_ref()).copied()
    }

    /// Set executable segment flags for Mach-O binaries.
    ///
    /// The incoming flags will replace any already defined flags for the path.
    pub fn set_executable_segment_flags(
        &mut self,
        scope: SettingsScope,
        flags: ExecutableSegmentFlags,
    ) {
        self.executable_segment_flags.insert(scope, flags);
    }

    /// Obtain the `Info.plist` data registered to a given scope.
    pub fn info_plist_data(&self, scope: impl AsRef<SettingsScope>) -> Option<&[u8]> {
        self.info_plist_data
            .get(scope.as_ref())
            .map(|x| x.as_slice())
    }

    /// Define the `Info.plist` content.
    ///
    /// Signatures can reference the digest of an external `Info.plist` file in
    /// the bundle the binary is located in.
    ///
    /// This function registers the raw content of that file is so that the
    /// content can be digested and the digest can be included in the code directory.
    ///
    /// The value passed here should be the raw content of the `Info.plist` XML file.
    ///
    /// When signing bundles, this function is called automatically with the `Info.plist`
    /// from the bundle. This function exists for cases where you are signing
    /// individual Mach-O binaries and the `Info.plist` cannot be automatically
    /// discovered.
    pub fn set_info_plist_data(&mut self, scope: SettingsScope, data: Vec<u8>) {
        self.info_plist_data.insert(scope, data);
    }

    /// Obtain the `CodeResources` XML file data registered to a given scope.
    pub fn code_resources_data(&self, scope: impl AsRef<SettingsScope>) -> Option<&[u8]> {
        self.code_resources_data
            .get(scope.as_ref())
            .map(|x| x.as_slice())
    }

    /// Define the `CodeResources` XML file content for a given scope.
    ///
    /// Bundles may contain a `CodeResources` XML file which defines additional
    /// resource files and binaries outside the bundle's main executable. The code
    /// directory of the main executable contains a digest of this file to establish
    /// a chain of trust of the content of this XML file.
    ///
    /// This function defines the content of this external file so that the content
    /// can be digested and that digest included in the code directory of the
    /// binary being signed.
    ///
    /// When signing bundles, this function is called automatically with the content
    /// of the `CodeResources` XML file, if present. This function exists for cases
    /// where you are signing individual Mach-O binaries and the `CodeResources` XML
    /// file cannot be automatically discovered.
    pub fn set_code_resources_data(&mut self, scope: SettingsScope, data: Vec<u8>) {
        self.code_resources_data.insert(scope, data);
    }

    /// Convert this instance to settings appropriate for a nested bundle.
    pub fn as_nested_bundle_settings(&self, bundle_path: &str) -> Self {
        self.clone_strip_prefix(bundle_path, format!("{}/", bundle_path))
    }

    /// Convert this instance to settings appropriate for a Mach-O binary in a bundle.
    pub fn as_bundle_macho_settings(&self, path: &str) -> Self {
        self.clone_strip_prefix(path, path.to_string())
    }

    /// Convert this instance to settings appropriate for a nested Mach-O binary.
    ///
    /// It is assumed the main scope of these settings is already targeted for
    /// a Mach-O binary. Any scoped settings for the Mach-O binary index and CPU type
    /// will be applied. CPU type settings take precedence over index scoped settings.
    pub fn as_nested_macho_settings(&self, index: usize, cpu_type: CpuType) -> Self {
        self.clone_with_filter_map(|key| {
            if key == SettingsScope::Main
                || key == SettingsScope::MultiArchCpuType(cpu_type)
                || key == SettingsScope::MultiArchIndex(index)
            {
                Some(SettingsScope::Main)
            } else {
                None
            }
        })
    }

    // Clones this instance, promoting `main_path` to the main scope and stripping
    // a prefix from other keys.
    fn clone_strip_prefix(&self, main_path: &str, prefix: String) -> Self {
        self.clone_with_filter_map(|key| match key {
            SettingsScope::Main => Some(SettingsScope::Main),
            SettingsScope::Path(path) => {
                if path == main_path {
                    Some(SettingsScope::Main)
                } else if let Some(path) = path.strip_prefix(&prefix) {
                    Some(SettingsScope::Path(path.to_string()))
                } else {
                    None
                }
            }
            SettingsScope::MultiArchIndex(index) => Some(SettingsScope::MultiArchIndex(index)),
            SettingsScope::MultiArchCpuType(cpu_type) => {
                Some(SettingsScope::MultiArchCpuType(cpu_type))
            }
            SettingsScope::PathMultiArchIndex(path, index) => {
                if path == main_path {
                    Some(SettingsScope::MultiArchIndex(index))
                } else if let Some(path) = path.strip_prefix(&prefix) {
                    Some(SettingsScope::PathMultiArchIndex(path.to_string(), index))
                } else {
                    None
                }
            }
            SettingsScope::PathMultiArchCpuType(path, cpu_type) => {
                if path == main_path {
                    Some(SettingsScope::MultiArchCpuType(cpu_type))
                } else if let Some(path) = path.strip_prefix(&prefix) {
                    Some(SettingsScope::PathMultiArchCpuType(
                        path.to_string(),
                        cpu_type,
                    ))
                } else {
                    None
                }
            }
        })
    }

    fn clone_with_filter_map(
        &self,
        key_map: impl Fn(SettingsScope) -> Option<SettingsScope>,
    ) -> Self {
        Self {
            signing_key: self.signing_key.clone(),
            certificates: self.certificates.clone(),
            time_stamp_url: self.time_stamp_url.clone(),
            team_name: self.team_name.clone(),
            digest_type: self.digest_type,
            identifiers: self
                .identifiers
                .clone()
                .into_iter()
                .filter_map(|(key, value)| {
                    if let Some(key) = key_map(key) {
                        Some((key, value))
                    } else {
                        None
                    }
                })
                .collect::<BTreeMap<_, _>>(),
            entitlements: self
                .entitlements
                .clone()
                .into_iter()
                .filter_map(|(key, value)| {
                    if let Some(key) = key_map(key) {
                        Some((key, value))
                    } else {
                        None
                    }
                })
                .collect::<BTreeMap<_, _>>(),
            designated_requirement: self
                .designated_requirement
                .clone()
                .into_iter()
                .filter_map(|(key, value)| {
                    if let Some(key) = key_map(key) {
                        Some((key, value))
                    } else {
                        None
                    }
                })
                .collect::<BTreeMap<_, _>>(),
            code_signature_flags: self
                .code_signature_flags
                .clone()
                .into_iter()
                .filter_map(|(key, value)| {
                    if let Some(key) = key_map(key) {
                        Some((key, value))
                    } else {
                        None
                    }
                })
                .collect::<BTreeMap<_, _>>(),
            executable_segment_flags: self
                .executable_segment_flags
                .clone()
                .into_iter()
                .filter_map(|(key, value)| {
                    if let Some(key) = key_map(key) {
                        Some((key, value))
                    } else {
                        None
                    }
                })
                .collect::<BTreeMap<_, _>>(),
            info_plist_data: self
                .info_plist_data
                .clone()
                .into_iter()
                .filter_map(|(key, value)| {
                    if let Some(key) = key_map(key) {
                        Some((key, value))
                    } else {
                        None
                    }
                })
                .collect::<BTreeMap<_, _>>(),
            code_resources_data: self
                .code_resources_data
                .clone()
                .into_iter()
                .filter_map(|(key, value)| {
                    if let Some(key) = key_map(key) {
                        Some((key, value))
                    } else {
                        None
                    }
                })
                .collect::<BTreeMap<_, _>>(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_settings_scope() {
        assert_eq!(
            SettingsScope::try_from("@main").unwrap(),
            SettingsScope::Main
        );
        assert_eq!(
            SettingsScope::try_from("@0").unwrap(),
            SettingsScope::MultiArchIndex(0)
        );
        assert_eq!(
            SettingsScope::try_from("@42").unwrap(),
            SettingsScope::MultiArchIndex(42)
        );
        assert_eq!(
            SettingsScope::try_from("@[cpu_type=7]").unwrap(),
            SettingsScope::MultiArchCpuType(7)
        );
        assert_eq!(
            SettingsScope::try_from("@[cpu_type=arm]").unwrap(),
            SettingsScope::MultiArchCpuType(CPU_TYPE_ARM)
        );
        assert_eq!(
            SettingsScope::try_from("@[cpu_type=arm64]").unwrap(),
            SettingsScope::MultiArchCpuType(CPU_TYPE_ARM64)
        );
        assert_eq!(
            SettingsScope::try_from("@[cpu_type=arm64_32]").unwrap(),
            SettingsScope::MultiArchCpuType(CPU_TYPE_ARM64_32)
        );
        assert_eq!(
            SettingsScope::try_from("@[cpu_type=x86_64]").unwrap(),
            SettingsScope::MultiArchCpuType(CPU_TYPE_X86_64)
        );
        assert_eq!(
            SettingsScope::try_from("foo/bar").unwrap(),
            SettingsScope::Path("foo/bar".into())
        );
        assert_eq!(
            SettingsScope::try_from("foo/bar@0").unwrap(),
            SettingsScope::PathMultiArchIndex("foo/bar".into(), 0)
        );
        assert_eq!(
            SettingsScope::try_from("foo/bar@[cpu_type=7]").unwrap(),
            SettingsScope::PathMultiArchCpuType("foo/bar".into(), 7_u32)
        );
    }

    #[test]
    fn as_nested_macho_settings() {
        let mut main_settings = SigningSettings::default();
        main_settings.set_binary_identifier(SettingsScope::Main, "ident");
        main_settings
            .set_code_signature_flags(SettingsScope::Main, CodeSignatureFlags::FORCE_EXPIRATION);

        main_settings.set_code_signature_flags(
            SettingsScope::MultiArchIndex(0),
            CodeSignatureFlags::FORCE_HARD,
        );
        main_settings.set_code_signature_flags(
            SettingsScope::MultiArchCpuType(CPU_TYPE_X86_64),
            CodeSignatureFlags::RESTRICT,
        );
        main_settings.set_entitlements_xml(SettingsScope::MultiArchIndex(0), "index_0");
        main_settings.set_entitlements_xml(
            SettingsScope::MultiArchCpuType(CPU_TYPE_X86_64),
            "cpu_x86_64",
        );

        let macho_settings = main_settings.as_nested_macho_settings(0, CPU_TYPE_ARM64);
        assert_eq!(
            macho_settings.binary_identifier(SettingsScope::Main),
            Some("ident")
        );
        assert_eq!(
            macho_settings.code_signature_flags(SettingsScope::Main),
            Some(CodeSignatureFlags::FORCE_HARD)
        );
        assert_eq!(
            macho_settings.entitlements_xml(SettingsScope::Main),
            Some("index_0")
        );

        let macho_settings = main_settings.as_nested_macho_settings(0, CPU_TYPE_X86_64);
        assert_eq!(
            macho_settings.binary_identifier(SettingsScope::Main),
            Some("ident")
        );
        assert_eq!(
            macho_settings.code_signature_flags(SettingsScope::Main),
            Some(CodeSignatureFlags::RESTRICT)
        );
        assert_eq!(
            macho_settings.entitlements_xml(SettingsScope::Main),
            Some("cpu_x86_64")
        );
    }

    #[test]
    fn as_bundle_macho_settings() {
        let mut main_settings = SigningSettings::default();
        main_settings.set_entitlements_xml(SettingsScope::Main, "main");
        main_settings.set_entitlements_xml(
            SettingsScope::Path("Contents/MacOS/main".into()),
            "main_exe",
        );
        main_settings.set_entitlements_xml(
            SettingsScope::PathMultiArchIndex("Contents/MacOS/main".into(), 0),
            "main_exe_index_0",
        );
        main_settings.set_entitlements_xml(
            SettingsScope::PathMultiArchCpuType("Contents/MacOS/main".into(), CPU_TYPE_X86_64),
            "main_exe_x86_64",
        );

        let macho_settings = main_settings.as_bundle_macho_settings("Contents/MacOS/main");
        assert_eq!(
            macho_settings.entitlements_xml(SettingsScope::Main),
            Some("main_exe")
        );
        assert_eq!(
            macho_settings.entitlements,
            [
                (SettingsScope::Main, "main_exe".into()),
                (SettingsScope::MultiArchIndex(0), "main_exe_index_0".into()),
                (
                    SettingsScope::MultiArchCpuType(CPU_TYPE_X86_64),
                    "main_exe_x86_64".into()
                ),
            ]
            .iter()
            .cloned()
            .collect::<BTreeMap<SettingsScope, String>>()
        );
    }

    #[test]
    fn as_nested_bundle_settings() {
        let mut main_settings = SigningSettings::default();
        main_settings.set_entitlements_xml(SettingsScope::Main, "main");
        main_settings.set_entitlements_xml(
            SettingsScope::Path("Contents/MacOS/main".into()),
            "main_exe",
        );
        main_settings.set_entitlements_xml(
            SettingsScope::Path("Contents/MacOS/nested.app".into()),
            "bundle",
        );
        main_settings.set_entitlements_xml(
            SettingsScope::PathMultiArchIndex("Contents/MacOS/nested.app".into(), 0),
            "bundle_index_0",
        );
        main_settings.set_entitlements_xml(
            SettingsScope::PathMultiArchCpuType(
                "Contents/MacOS/nested.app".into(),
                CPU_TYPE_X86_64,
            ),
            "bundle_x86_64",
        );
        main_settings.set_entitlements_xml(
            SettingsScope::Path("Contents/MacOS/nested.app/Contents/MacOS/nested".into()),
            "nested_main_exe",
        );
        main_settings.set_entitlements_xml(
            SettingsScope::PathMultiArchIndex(
                "Contents/MacOS/nested.app/Contents/MacOS/nested".into(),
                0,
            ),
            "nested_main_exe_index_0",
        );
        main_settings.set_entitlements_xml(
            SettingsScope::PathMultiArchCpuType(
                "Contents/MacOS/nested.app/Contents/MacOS/nested".into(),
                CPU_TYPE_X86_64,
            ),
            "nested_main_exe_x86_64",
        );

        let bundle_settings = main_settings.as_nested_bundle_settings("Contents/MacOS/nested.app");
        assert_eq!(
            bundle_settings.entitlements_xml(SettingsScope::Main),
            Some("bundle")
        );
        assert_eq!(
            bundle_settings.entitlements_xml(SettingsScope::Path("Contents/MacOS/nested".into())),
            Some("nested_main_exe")
        );
        assert_eq!(
            bundle_settings.entitlements,
            [
                (SettingsScope::Main, "bundle".into()),
                (SettingsScope::MultiArchIndex(0), "bundle_index_0".into()),
                (
                    SettingsScope::MultiArchCpuType(CPU_TYPE_X86_64),
                    "bundle_x86_64".into()
                ),
                (
                    SettingsScope::Path("Contents/MacOS/nested".into()),
                    "nested_main_exe".into()
                ),
                (
                    SettingsScope::PathMultiArchIndex("Contents/MacOS/nested".into(), 0),
                    "nested_main_exe_index_0".into()
                ),
                (
                    SettingsScope::PathMultiArchCpuType(
                        "Contents/MacOS/nested".into(),
                        CPU_TYPE_X86_64
                    ),
                    "nested_main_exe_x86_64".into()
                ),
            ]
            .iter()
            .cloned()
            .collect::<BTreeMap<SettingsScope, String>>()
        );
    }
}
