// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Signing binaries.
//!
//! This module contains code for signing binaries.

use {
    crate::{
        code_directory::{CodeDirectoryBlob, CodeSignatureFlags},
        code_hash::compute_code_hashes,
        code_requirement::{CodeRequirementExpression, CodeRequirements},
        error::AppleCodesignError,
        macho::{
            create_superblob, find_signature_data, parse_signature_data, Blob, BlobWrapperBlob,
            CodeSigningMagic, CodeSigningSlot, Digest, DigestType, EmbeddedSignature,
            EntitlementsBlob, RequirementSetBlob, RequirementType,
        },
        signing::{SettingsScope, SigningSettings},
    },
    bytes::Bytes,
    cryptographic_message_syntax::{SignedDataBuilder, SignerBuilder},
    goblin::mach::{
        constants::{SEG_LINKEDIT, SEG_PAGEZERO},
        fat::FAT_MAGIC,
        fat::{SIZEOF_FAT_ARCH, SIZEOF_FAT_HEADER},
        load_command::{CommandVariant, LinkeditDataCommand, SegmentCommand32, SegmentCommand64},
        parse_magic_and_ctx, Mach, MachO,
    },
    scroll::{ctx::SizeWith, IOwrite, Pwrite},
    std::{borrow::Cow, cmp::Ordering, collections::HashMap, io::Write},
};

/// OID for signed attribute containing plist of code directory hashes.
///
/// 1.2.840.113635.100.9.1.
const CDHASH_PLIST_OID: bcder::ConstOid = bcder::Oid(&[42, 134, 72, 134, 247, 99, 100, 9, 1]);

/// Determines whether this crate is capable of signing a given Mach-O binary.
///
/// Code in this crate is limited in the amount of Mach-O binary manipulation
/// it can perform (supporting rewriting all valid Mach-O binaries effectively
/// requires low-level awareness of all Mach-O constructs in order to perform
/// offset manipulation). This function can be used to test signing
/// compatibility.
///
/// We currently only support signing Mach-O files already containing an
/// embedded signature. Often linked binaries automatically contain an embedded
/// signature containing just the code directory (without a cryptographically
/// signed signature), so this limitation hopefully isn't impactful.
pub fn check_signing_capability(macho: &MachO) -> Result<(), AppleCodesignError> {
    match find_signature_data(macho)? {
        Some(signature) => {
            // __LINKEDIT needs to be the final segment so we don't have to rewrite
            // offsets.
            if signature.linkedit_segment_index != macho.segments.len() - 1 {
                Err(AppleCodesignError::LinkeditNotLast)
            // There can be no meaningful data after the signature because we don't
            // know how to rewrite it.
            } else if signature.signature_end_offset != signature.linkedit_segment_data.len() {
                Err(AppleCodesignError::DataAfterSignature)
            } else {
                Ok(())
            }
        }
        None => Err(AppleCodesignError::BinaryNoCodeSignature),
    }
}

/// Obtain the XML plist containing code directory hashes.
///
/// This plist is embedded as a signed attribute in the CMS signature.
pub fn create_code_directory_hashes_plist<'a>(
    code_directories: impl Iterator<Item = &'a CodeDirectoryBlob<'a>>,
    digest_type: DigestType,
) -> Result<Vec<u8>, AppleCodesignError> {
    let hashes = code_directories
        .map(|cd| {
            let blob_data = cd.to_blob_bytes()?;

            let digest = digest_type.digest(&blob_data)?;

            Ok(plist::Value::String(base64::encode(&digest)))
        })
        .collect::<Result<Vec<_>, AppleCodesignError>>()?;

    let mut plist = plist::Dictionary::new();
    plist.insert("cdhashes".to_string(), plist::Value::Array(hashes));

    let mut buffer = Vec::<u8>::new();
    plist::Value::from(plist)
        .to_writer_xml(&mut buffer)
        .map_err(AppleCodesignError::CodeDirectoryPlist)?;

    Ok(buffer)
}

/// Derive a new Mach-O binary with new signature data.
fn create_macho_with_signature(
    macho_data: &[u8],
    macho: &MachO,
    signature_data: &[u8],
) -> Result<Vec<u8>, AppleCodesignError> {
    let existing_signature =
        find_signature_data(macho)?.ok_or(AppleCodesignError::BinaryNoCodeSignature)?;

    // This should have already been called. But we do it again out of paranoia.
    check_signing_capability(macho)?;

    // The assumption made by checking_signing_capability() is that signature data
    // is at the end of the __LINKEDIT segment. So the replacement segment is the
    // existing segment truncated at the signature start followed by the new signature
    // data.
    let new_linkedit_segment_size =
        existing_signature.signature_start_offset + signature_data.len();

    let mut cursor = std::io::Cursor::new(Vec::<u8>::new());

    // Mach-O data structures are variable endian. So use the endian defined
    // by the magic when writing.
    let ctx = parse_magic_and_ctx(&macho_data, 0)?
        .1
        .expect("context should have been parsed before");

    cursor.iowrite_with(macho.header, ctx)?;

    // Following the header are load commands. We need to update load commands
    // to reflect changes to the signature size and __LINKEDIT segment size.
    for load_command in &macho.load_commands {
        let original_command_data =
            &macho_data[load_command.offset..load_command.offset + load_command.command.cmdsize()];

        let written_len = match &load_command.command {
            CommandVariant::CodeSignature(command) => {
                let mut command = *command;
                command.datasize = signature_data.len() as _;

                cursor.iowrite_with(command, ctx.le)?;

                LinkeditDataCommand::size_with(&ctx.le)
            }
            CommandVariant::Segment32(segment) => {
                let segment = match segment.name() {
                    Ok(SEG_LINKEDIT) => {
                        let mut segment = *segment;
                        segment.filesize = new_linkedit_segment_size as _;

                        segment
                    }
                    _ => *segment,
                };

                cursor.iowrite_with(segment, ctx.le)?;

                SegmentCommand32::size_with(&ctx.le)
            }
            CommandVariant::Segment64(segment) => {
                let segment = match segment.name() {
                    Ok(SEG_LINKEDIT) => {
                        let mut segment = *segment;
                        segment.filesize = new_linkedit_segment_size as _;

                        segment
                    }
                    _ => *segment,
                };

                cursor.iowrite_with(segment, ctx.le)?;

                SegmentCommand64::size_with(&ctx.le)
            }
            _ => {
                // Reflect the original bytes.
                cursor.write_all(original_command_data)?;
                original_command_data.len()
            }
        };

        // For the commands we mutated ourselves, there may be more data after the
        // load command header. Write it out if present.
        cursor.write_all(&original_command_data[written_len..])?;
    }

    // Write out segments, updating the __LINKEDIT segment when we encounter it.
    for segment in macho.segments.iter() {
        assert!(segment.fileoff == 0 || segment.fileoff == cursor.position());

        // The initial __PAGEZERO segment contains no data (it is the magic and load
        // commands) and overlaps with the __TEXT segment, which has .fileoff =0, so
        // we ignore it.
        if matches!(segment.name(), Ok(SEG_PAGEZERO)) {
            continue;
        }

        match segment.name() {
            Ok(SEG_LINKEDIT) => {
                cursor.write_all(
                    &existing_signature.linkedit_segment_data
                        [0..existing_signature.signature_start_offset],
                )?;
                cursor.write_all(signature_data)?;
            }
            _ => {
                // At least the __TEXT segment has .fileoff = 0, which has it
                // overlapping with already written data. So only write segment
                // data new to the writer.
                if segment.fileoff < cursor.position() {
                    let remaining =
                        &segment.data[cursor.position() as usize..segment.filesize as usize];
                    cursor.write_all(remaining)?;
                } else {
                    cursor.write_all(segment.data)?;
                }
            }
        }
    }

    Ok(cursor.into_inner())
}

/// Mach-O binary signer.
///
/// This type provides a high-level interface for signing Mach-O binaries.
/// It handles parsing and rewriting Mach-O binaries and contains most of the
/// functionality for producing signatures for individual Mach-O binaries.
///
/// Signing of both single architecture and fat/universal binaries is supported.
///
/// # Circular Dependency
///
/// There is a circular dependency between the generation of the Code Directory
/// present in the embedded signature and the Mach-O binary. See the note
/// in [crate::specification] for the gory details. The tl;dr is the Mach-O
/// data up to the signature data needs to be digested. But that digested data
/// contains load commands that reference the signature data and its size, which
/// can't be known until the Code Directory, CMS blob, and SuperBlob are all
/// created.
///
/// Our solution to this problem is to create an intermediate Mach-O binary with
/// placeholder bytes for the signature. We then digest this. When writing
/// the final Mach-O binary we simply replace NULLs with actual signature data,
/// leaving any extra at the end, because truncating the file would require
/// adjusting Mach-O load commands and changing content digests.
#[derive(Debug)]
pub struct MachOSigner<'data> {
    /// Raw data backing parsed Mach-O binary.
    macho_data: &'data [u8],

    /// Parsed Mach-O binaries.
    machos: Vec<MachO<'data>>,
}

impl<'data> MachOSigner<'data> {
    /// Construct a new instance from unparsed data representing a Mach-O binary.
    ///
    /// The data will be parsed as a Mach-O binary (either single arch or fat/universal)
    /// and validated that we are capable of signing it.
    pub fn new(macho_data: &'data [u8]) -> Result<Self, AppleCodesignError> {
        let mach = Mach::parse(macho_data)?;

        let machos = match mach {
            Mach::Binary(macho) => {
                check_signing_capability(&macho)?;

                vec![macho]
            }
            Mach::Fat(multiarch) => {
                let mut machos = vec![];

                for index in 0..multiarch.narches {
                    let macho = multiarch.get(index)?;
                    check_signing_capability(&macho)?;

                    machos.push(macho);
                }

                machos
            }
        };

        Ok(Self { macho_data, machos })
    }

    /// Write signed Mach-O data to the given writer using signing settings.
    pub fn write_signed_binary(
        &self,
        settings: &SigningSettings,
        writer: &mut impl Write,
    ) -> Result<(), AppleCodesignError> {
        // Implementing a true streaming writer requires calculating final sizes
        // of all binaries so fat header offsets and sizes can be written first. We take
        // the easy road and buffer individual Mach-O binaries internally.

        let binaries = self
            .machos
            .iter()
            .enumerate()
            .map(|(index, original_macho)| {
                let settings =
                    settings.as_nested_macho_settings(index, original_macho.header.cputype());

                let signature_data = find_signature_data(original_macho)?;
                let signature = if let Some(data) = &signature_data {
                    Some(parse_signature_data(&data.signature_data)?)
                } else {
                    None
                };

                // Derive an intermediate Mach-O with placeholder NULLs for signature
                // data so Code Directory digests are correct.
                let placeholder_signature_len = self
                    .create_superblob(&settings, original_macho, signature.as_ref())?
                    .len();
                let placeholder_signature = b"\0".repeat(placeholder_signature_len + 1024);

                // TODO calling this twice could be undesirable, especially if using
                // a timestamp server. Should we call in no-op mode or write a size
                // estimation function instead?
                let intermediate_macho_data = create_macho_with_signature(
                    self.macho_data(index),
                    original_macho,
                    &placeholder_signature,
                )?;

                // A nice side-effect of this is that it catches bugs if we write malformed Mach-O!
                let intermediate_macho = MachO::parse(&intermediate_macho_data, 0)?;

                let mut signature_data =
                    self.create_superblob(&settings, &intermediate_macho, signature.as_ref())?;

                // The Mach-O writer adjusts load commands based on the signature length. So pad
                // with NULLs to get to our placeholder length.
                match signature_data.len().cmp(&placeholder_signature.len()) {
                    Ordering::Greater => {
                        return Err(AppleCodesignError::SignatureDataTooLarge);
                    }
                    Ordering::Equal => {}
                    Ordering::Less => {
                        signature_data.extend_from_slice(
                            &b"\0".repeat(placeholder_signature.len() - signature_data.len()),
                        );
                    }
                }

                create_macho_with_signature(
                    &intermediate_macho_data,
                    &intermediate_macho,
                    &signature_data,
                )
            })
            .collect::<Result<Vec<_>, AppleCodesignError>>()?;

        match Mach::parse(&self.macho_data).expect("should reparse without error") {
            Mach::Binary(_) => {
                assert_eq!(binaries.len(), 1);
                writer.write_all(&binaries[0])?;
            }
            Mach::Fat(multiarch) => {
                assert_eq!(binaries.len(), multiarch.narches);

                // The fat arch header records the start offset and size of each binary.
                // Do a pass over the binaries and calculate these offsets.
                //
                // Binaries appear to be 4k page aligned, so also collect padding
                // information so we write nulls later.
                let mut current_offset = SIZEOF_FAT_HEADER + SIZEOF_FAT_ARCH * binaries.len();
                let mut write_instructions = Vec::with_capacity(binaries.len());

                for (index, arch) in multiarch.iter_arches().enumerate() {
                    let mut arch = arch?;
                    let macho_data = &binaries[index];

                    let pad_bytes = 4096 - current_offset % 4096;

                    arch.offset = (current_offset + pad_bytes) as _;
                    arch.size = macho_data.len() as _;

                    current_offset += macho_data.len() + pad_bytes;

                    write_instructions.push((arch, pad_bytes, macho_data));
                }

                writer.iowrite_with(FAT_MAGIC, scroll::BE)?;
                writer.iowrite_with(multiarch.narches as u32, scroll::BE)?;

                for (fat_arch, _, _) in &write_instructions {
                    let mut buffer = [0u8; SIZEOF_FAT_ARCH];
                    buffer.pwrite_with(fat_arch, 0, scroll::BE)?;
                    writer.write_all(&buffer)?;
                }

                for (_, pad_bytes, macho_data) in write_instructions {
                    writer.write_all(&b"\0".repeat(pad_bytes))?;
                    writer.write_all(macho_data)?;
                }
            }
        }

        Ok(())
    }

    /// Derive the data slice belonging to a Mach-O binary.
    fn macho_data(&self, index: usize) -> &[u8] {
        match Mach::parse(&self.macho_data).expect("should reparse without error") {
            Mach::Binary(_) => &self.macho_data,
            Mach::Fat(multiarch) => {
                let arch = multiarch
                    .iter_arches()
                    .nth(index)
                    .expect("bad index")
                    .expect("reparse should have worked");

                let end_offset = arch.offset as usize + arch.size as usize;

                &self.macho_data[arch.offset as usize..end_offset]
            }
        }
    }

    /// Create data constituting the SuperBlob to be embedded in the `__LINKEDIT` segment.
    ///
    /// The superblob contains the code directory, any extra blobs, and an optional
    /// CMS structure containing a cryptographic signature.
    ///
    /// This takes an explicit Mach-O to operate on due to a circular dependency
    /// between writing out the Mach-O and digesting its content. See the note
    /// in [MachOSigner] for details.
    pub fn create_superblob(
        &self,
        settings: &SigningSettings,
        macho: &MachO,
        signature: Option<&EmbeddedSignature>,
    ) -> Result<Vec<u8>, AppleCodesignError> {
        let code_directory = self.create_code_directory(settings, macho, signature)?;

        // By convention, the Code Directory goes first.
        let mut blobs = vec![(
            CodeSigningSlot::CodeDirectory,
            code_directory.to_blob_bytes()?,
        )];
        blobs.extend(self.create_special_blobs(settings)?);

        // And the CMS signature goes last.
        if settings.signing_key().is_some() {
            blobs.push((
                CodeSigningSlot::Signature,
                BlobWrapperBlob::from_data(&self.create_cms_signature(settings, &code_directory)?)
                    .to_blob_bytes()?,
            ));
        }

        create_superblob(CodeSigningMagic::EmbeddedSignature, blobs.iter())
    }

    /// Create a CMS `SignedData` structure containing a cryptographic signature.
    ///
    /// This becomes the content of the `EmbeddedSignature` blob in the `Signature` slot.
    ///
    /// This function will error if a signing key has not been specified.
    ///
    /// This takes an explicit Mach-O to operate on due to a circular dependency
    /// between writing out the Mach-O and digesting its content. See the note
    /// in [MachOSigner] for details.
    pub fn create_cms_signature(
        &self,
        settings: &SigningSettings,
        code_directory: &CodeDirectoryBlob,
    ) -> Result<Vec<u8>, AppleCodesignError> {
        let (signing_key, signing_cert) = settings
            .signing_key()
            .ok_or(AppleCodesignError::NoSigningCertificate)?;

        // We need the blob serialized content of the code directory to compute
        // the message digest using alternate data.
        let code_directory_raw = code_directory.to_blob_bytes()?;

        // We need an XML plist containing code directory hashes to include as a signed
        // attribute.
        let code_directories = vec![code_directory];
        let code_directory_hashes_plist = create_code_directory_hashes_plist(
            code_directories.into_iter(),
            code_directory.hash_type,
        )?;

        let signer = SignerBuilder::new(signing_key, signing_cert.clone())
            .message_id_content(code_directory_raw)
            .signed_attribute_octet_string(
                bcder::Oid(Bytes::copy_from_slice(CDHASH_PLIST_OID.as_ref())),
                &code_directory_hashes_plist,
            );
        let signer = if let Some(time_stamp_url) = settings.time_stamp_url() {
            signer.time_stamp_url(time_stamp_url.clone())?
        } else {
            signer
        };

        let ber = SignedDataBuilder::default()
            .signer(signer)
            .certificates(settings.certificate_chain().iter().cloned())?
            .build_ber()?;

        Ok(ber)
    }

    /// Create the `CodeDirectory` for the current configuration.
    ///
    /// This takes an explicit Mach-O to operate on due to a circular dependency
    /// between writing out the Mach-O and digesting its content. See the note
    /// in [MachOSigner] for details.
    pub fn create_code_directory(
        &self,
        settings: &SigningSettings,
        macho: &MachO,
        signature: Option<&EmbeddedSignature>,
    ) -> Result<CodeDirectoryBlob<'static>, AppleCodesignError> {
        // TODO support defining or filling in proper values for fields with
        // static values.

        let previous_cd =
            signature.and_then(|signature| signature.code_directory().unwrap_or(None));

        let signature_data = find_signature_data(macho)?;

        let mut flags = CodeSignatureFlags::empty();

        match settings.code_signature_flags(SettingsScope::Main) {
            Some(additional) => flags |= additional,
            None => {
                if let Some(previous_cd) = &previous_cd {
                    flags |= previous_cd.flags;
                }
            }
        }

        // The adhoc flag is set when there is no CMS signature.
        if settings.signing_key().is_none() {
            flags |= CodeSignatureFlags::ADHOC;
        } else {
            flags -= CodeSignatureFlags::ADHOC;
        }

        // Remove linker signed flag because we're not a linker.
        flags -= CodeSignatureFlags::LINKER_SIGNED;

        // Code limit fields hold the file offset at which code digests stop. This
        // is the file offset in the `__LINKEDIT` segment when the embedded signature
        // SuperBlob begins.
        let (code_limit, code_limit_64) = match &signature_data {
            Some(sig) => {
                // If binary already has signature data, take existing signature start offset.
                let limit = sig.linkedit_signature_start_offset;

                if limit > u32::MAX as usize {
                    (0, Some(limit as u64))
                } else {
                    (limit as u32, None)
                }
            }
            None => {
                // No existing signature in binary. Look for __LINKEDIT and use its
                // end offset.
                match macho
                    .segments
                    .iter()
                    .find(|x| matches!(x.name(), Ok("__LINKEDIT")))
                {
                    Some(segment) => {
                        let limit = segment.fileoff as usize + segment.data.len();

                        if limit > u32::MAX as usize {
                            (0, Some(limit as u64))
                        } else {
                            (limit as u32, None)
                        }
                    }
                    None => {
                        let last_segment = macho.segments.iter().last().unwrap();
                        let limit = last_segment.fileoff as usize + last_segment.data.len();

                        if limit > u32::MAX as usize {
                            (0, Some(limit as u64))
                        } else {
                            (limit as u32, None)
                        }
                    }
                }
            }
        };

        let platform = 0;
        let page_size = 4096u32;

        let mut exec_seg_flags = None;

        match settings.executable_segment_flags(SettingsScope::Main) {
            Some(flags) => {
                exec_seg_flags = Some(flags);
            }
            None => {
                if let Some(previous_cd) = &previous_cd {
                    if let Some(flags) = previous_cd.exec_seg_flags {
                        exec_seg_flags = Some(flags);
                    }
                }
            }
        }

        let runtime = match &previous_cd {
            Some(previous_cd) => previous_cd.runtime,
            None => None,
        };

        let code_hashes =
            compute_code_hashes(macho, *settings.digest_type(), Some(page_size as usize))?
                .into_iter()
                .map(|v| Digest { data: v.into() })
                .collect::<Vec<_>>();

        let mut special_hashes = self
            .create_special_blobs(settings)?
            .into_iter()
            .map(|(slot, data)| {
                Ok((
                    slot,
                    Digest {
                        data: settings.digest_type().digest(&data)?.into(),
                    },
                ))
            })
            .collect::<Result<HashMap<_, _>, AppleCodesignError>>()?;

        match settings.info_plist_data(SettingsScope::Main) {
            Some(data) => {
                special_hashes.insert(
                    CodeSigningSlot::Info,
                    Digest {
                        data: settings.digest_type().digest(data)?.into(),
                    },
                );
            }
            None => {
                if let Some(previous_cd) = &previous_cd {
                    if let Some(digest) = previous_cd.special_hashes.get(&CodeSigningSlot::Info) {
                        if !digest.is_null() {
                            special_hashes.insert(CodeSigningSlot::Info, digest.to_owned());
                        }
                    }
                }
            }
        }

        match settings.code_resources_data(SettingsScope::Main) {
            Some(data) => {
                special_hashes.insert(
                    CodeSigningSlot::ResourceDir,
                    Digest {
                        data: settings.digest_type().digest(data)?.into(),
                    }
                    .to_owned(),
                );
            }
            None => {
                if let Some(previous_cd) = &previous_cd {
                    if let Some(digest) = previous_cd
                        .special_hashes
                        .get(&CodeSigningSlot::ResourceDir)
                    {
                        if !digest.is_null() {
                            special_hashes.insert(CodeSigningSlot::ResourceDir, digest.to_owned());
                        }
                    }
                }
            }
        }

        let ident = Cow::Owned(match settings.binary_identifier(SettingsScope::Main) {
            Some(ident) => ident.to_string(),
            None => {
                if let Some(previous_cd) = &previous_cd {
                    previous_cd.ident.to_string()
                } else {
                    return Err(AppleCodesignError::NoIdentifier);
                }
            }
        });

        let team_name = match settings.team_name() {
            Some(team_name) => Some(Cow::Owned(team_name.to_string())),
            None => {
                if let Some(previous_cd) = &previous_cd {
                    if let Some(name) = &previous_cd.team_name {
                        Some(Cow::Owned(name.clone().into_owned()))
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
        };

        let mut cd = CodeDirectoryBlob {
            version: 0,
            flags,
            code_limit,
            hash_size: settings.digest_type().hash_len()? as u8,
            hash_type: *settings.digest_type(),
            platform,
            page_size,
            spare2: 0,
            scatter_offset: None,
            spare3: None,
            code_limit_64,
            exec_seg_base: None,
            exec_seg_limit: None,
            exec_seg_flags,
            runtime,
            pre_encrypt_offset: None,
            linkage_hash_type: None,
            linkage_truncated: None,
            spare4: None,
            linkage_offset: None,
            linkage_size: None,
            ident,
            team_name,
            code_hashes,
            special_hashes,
        };

        cd.adjust_version();
        cd.clear_newer_fields();

        Ok(cd)
    }

    /// Create blobs that need to be written given the current configuration.
    ///
    /// This emits all blobs except `CodeDirectory` and `Signature`, which are
    /// special since they are derived from the blobs emitted here.
    ///
    /// The goal of this function is to emit data to facilitate the creation of
    /// a `CodeDirectory`, which requires hashing blobs.
    pub fn create_special_blobs(
        &self,
        settings: &SigningSettings,
    ) -> Result<Vec<(CodeSigningSlot, Vec<u8>)>, AppleCodesignError> {
        let mut res = Vec::new();

        if let Some(exprs) = settings.designated_requirement(SettingsScope::Main) {
            let mut requirements = CodeRequirements::default();

            for expr in exprs {
                requirements.push(CodeRequirementExpression::from_bytes(expr)?.0);
            }

            let mut blob = RequirementSetBlob::default();
            requirements.add_to_requirement_set(&mut blob, RequirementType::Designated)?;

            res.push((CodeSigningSlot::RequirementSet, blob.to_blob_bytes()?));
        }

        if let Some(entitlements) = settings.entitlements_xml(SettingsScope::Main) {
            let blob = EntitlementsBlob::from_string(entitlements);

            res.push((CodeSigningSlot::Entitlements, blob.to_blob_bytes()?));
        }

        Ok(res)
    }
}
