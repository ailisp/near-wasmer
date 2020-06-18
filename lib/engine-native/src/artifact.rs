//! Define `NativeArtifact` to allow compiling and instantiating to be
//! done as separate steps.

use crate::engine::{NativeEngine, NativeEngineInner};
use crate::serialize::ModuleMetadata;
use libloading::{Library, Symbol as LibrarySymbol};
#[cfg(feature = "compiler")]
use object::write::{Object, Relocation, StandardSection, Symbol, SymbolSection};
#[cfg(feature = "compiler")]
use object::{RelocationEncoding, RelocationKind, SymbolFlags, SymbolKind, SymbolScope};
use std::error::Error;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
#[cfg(feature = "compiler")]
use std::process::Command;
use std::sync::Arc;
use tempfile::NamedTempFile;
#[cfg(feature = "compiler")]
use tracing::trace;
use wasm_common::entity::{BoxedSlice, PrimaryMap};
use wasm_common::{
    FunctionIndex, LocalFunctionIndex, MemoryIndex, OwnedDataInitializer, SignatureIndex,
    TableIndex,
};
#[cfg(feature = "compiler")]
use wasmer_compiler::{
    Architecture, BinaryFormat, CustomSectionProtection, Endianness, ModuleEnvironment,
    OperatingSystem, RelocationTarget, Triple,
};
use wasmer_compiler::{CompileError, CompileModuleInfo, Features};
#[cfg(feature = "compiler")]
use wasmer_engine::Engine;
use wasmer_engine::{
    Artifact, DeserializeError, InstantiationError, LinkError, RuntimeError, SerializeError,
    Tunables,
};
use wasmer_runtime::{MemoryPlan, TablePlan};
use wasmer_runtime::{ModuleInfo, VMFunctionBody, VMSharedSignatureIndex, VMTrampoline};

/// A compiled wasm module, ready to be instantiated.
pub struct NativeArtifact {
    sharedobject_path: PathBuf,
    metadata: ModuleMetadata,
    #[allow(dead_code)]
    library: Option<Library>,
    finished_functions: BoxedSlice<LocalFunctionIndex, *mut [VMFunctionBody]>,
    finished_dynamic_function_trampolines: BoxedSlice<FunctionIndex, *mut [VMFunctionBody]>,
    signatures: BoxedSlice<SignatureIndex, VMSharedSignatureIndex>,
}

fn to_compile_error(err: impl Error) -> CompileError {
    CompileError::Codegen(format!("{}", err))
}

impl NativeArtifact {
    // Mach-O header in Mac
    #[allow(dead_code)]
    const MAGIC_HEADER_MH_CIGAM_64: &'static [u8] = &[207, 250, 237, 254];

    // ELF Magic header for Linux (32 bit)
    #[allow(dead_code)]
    const MAGIC_HEADER_ELF_32: &'static [u8] = &[0x7f, b'E', b'L', b'F', 1];

    // ELF Magic header for Linux (64 bit)
    #[allow(dead_code)]
    const MAGIC_HEADER_ELF_64: &'static [u8] = &[0x7f, b'E', b'L', b'F', 2];

    // COFF Magic header for Windows (64 bit)
    #[allow(dead_code)]
    const MAGIC_HEADER_COFF_64: &'static [u8] = &[b'M', b'Z'];

    /// Check if the provided bytes look like `NativeArtifact`.
    ///
    /// This means, if the bytes look like a shared object file in the target
    /// system.
    pub fn is_deserializable(bytes: &[u8]) -> bool {
        cfg_if::cfg_if! {
            if #[cfg(all(target_pointer_width = "64", target_os="macos"))] {
                bytes.starts_with(Self::MAGIC_HEADER_MH_CIGAM_64)
            }
            else if #[cfg(all(target_pointer_width = "64", target_os="linux"))] {
                bytes.starts_with(Self::MAGIC_HEADER_ELF_64)
            }
            else if #[cfg(all(target_pointer_width = "32", target_os="linux"))] {
                bytes.starts_with(Self::MAGIC_HEADER_ELF_32)
            }
            else if #[cfg(all(target_pointer_width = "64", target_os="windows"))] {
                bytes.starts_with(Self::MAGIC_HEADER_COFF_64)
            }
            else {
                false
            }
        }
    }

    /// Compile a data buffer into a `NativeArtifact`, which may then be instantiated.
    #[cfg(feature = "compiler")]
    pub fn new(
        engine: &NativeEngine,
        data: &[u8],
        tunables: &dyn Tunables,
    ) -> Result<Self, CompileError> {
        let environ = ModuleEnvironment::new();
        let mut engine_inner = engine.inner_mut();

        let translation = environ.translate(data).map_err(CompileError::Wasm)?;
        let features = engine_inner.features();
        let memory_plans: PrimaryMap<MemoryIndex, MemoryPlan> = translation
            .module
            .memories
            .values()
            .map(|memory_type| tunables.memory_plan(*memory_type))
            .collect();
        let table_plans: PrimaryMap<TableIndex, TablePlan> = translation
            .module
            .tables
            .values()
            .map(|table_type| tunables.table_plan(*table_type))
            .collect();

        let compile_info = CompileModuleInfo {
            module: Arc::new(translation.module),
            features: features.clone(),
            memory_plans,
            table_plans,
        };

        let compiler = engine_inner.compiler()?;
        let target = engine.target();

        // Compile the Module
        let compilation = compiler.compile_module(
            &target,
            &compile_info,
            translation.module_translation.as_ref().unwrap(),
            translation.function_body_inputs,
        )?;
        let function_call_trampolines = compilation.get_function_call_trampolines();
        let dynamic_function_trampolines = compilation.get_dynamic_function_trampolines();

        let data_initializers = translation
            .data_initializers
            .iter()
            .map(OwnedDataInitializer::new)
            .collect::<Vec<_>>()
            .into_boxed_slice();

        let target_triple = target.triple().clone();

        let obj_binary_format = match target_triple.binary_format {
            BinaryFormat::Elf => object::BinaryFormat::Elf,
            BinaryFormat::Macho => object::BinaryFormat::MachO,
            BinaryFormat::Coff => object::BinaryFormat::Coff,
            format => {
                return Err(CompileError::Codegen(format!(
                    "Binary format {} not supported",
                    format
                )))
            }
        };
        let obj_architecture = match target_triple.architecture {
            Architecture::X86_64 => object::Architecture::X86_64,
            Architecture::Aarch64(_) => object::Architecture::Aarch64,
            architecture => {
                return Err(CompileError::Codegen(format!(
                    "Architecture {} not supported",
                    architecture
                )))
            }
        };
        let obj_endianness = match target_triple.endianness() {
            Ok(Endianness::Little) => object::Endianness::Little,
            Ok(Endianness::Big) => object::Endianness::Big,
            Err(e) => {
                return Err(CompileError::Codegen(format!(
                    "Can't detect the endianness for the target: {:?}",
                    e
                )))
            }
        };
        let mut obj = Object::new(obj_binary_format, obj_architecture, obj_endianness);
        let function_bodies = compilation.get_function_bodies();
        let custom_sections = compilation.get_custom_sections();
        let custom_section_relocations = compilation.get_custom_section_relocations();

        // We construct the function body lengths
        let function_body_lengths = function_bodies
            .values()
            .map(|function_body| function_body.body.len() as u64)
            .collect::<PrimaryMap<LocalFunctionIndex, u64>>();

        let metadata = ModuleMetadata {
            compile_info,
            prefix: engine_inner.get_prefix(&data),
            data_initializers,
            function_body_lengths,
        };

        let serialized_data = bincode::serialize(&metadata).map_err(to_compile_error)?;

        let mut metadata_length = vec![0; 10];
        let mut writable = &mut metadata_length[..];
        leb128::write::unsigned(&mut writable, serialized_data.len() as u64)
            .expect("Should write number");
        metadata_length.extend(serialized_data);

        let symbol_id = obj.add_symbol(Symbol {
            name: b"WASMER_METADATA".to_vec(),
            value: 0,
            size: 0,
            kind: SymbolKind::Data,
            scope: SymbolScope::Dynamic,
            weak: false,
            section: SymbolSection::Undefined,
            flags: SymbolFlags::None,
        });
        let section_id = obj.section_id(StandardSection::Data);
        obj.add_symbol_data(symbol_id, section_id, &metadata_length, 1);

        let function_relocations = compilation.get_relocations();

        // Add sections
        for (section_index, custom_section) in custom_sections.iter() {
            // TODO: We need to rename the sections corresponding to the DWARF information
            // to the proper names (like `.eh_frame`)
            let section_name = metadata.get_section_name(section_index);
            let (section_kind, standard_section) = match custom_section.protection {
                CustomSectionProtection::ReadExecute => (SymbolKind::Text, StandardSection::Text),
                // TODO: Fix this to be StandardSection::Data
                CustomSectionProtection::Read => (SymbolKind::Data, StandardSection::Text),
            };
            let symbol_id = obj.add_symbol(Symbol {
                name: section_name.as_bytes().to_vec(),
                value: 0,
                size: 0,
                kind: section_kind,
                scope: SymbolScope::Dynamic,
                weak: false,
                section: SymbolSection::Undefined,
                flags: SymbolFlags::None,
            });
            let section_id = obj.section_id(standard_section);
            obj.add_symbol_data(symbol_id, section_id, custom_section.bytes.as_slice(), 1);
        }

        // Add functions
        for (function_local_index, function) in function_bodies.into_iter() {
            let function_name = metadata.get_function_name(function_local_index);
            let symbol_id = obj.add_symbol(Symbol {
                name: function_name.as_bytes().to_vec(),
                value: 0,
                size: 0,
                kind: SymbolKind::Text,
                scope: SymbolScope::Dynamic,
                weak: false,
                section: SymbolSection::Undefined,
                flags: SymbolFlags::None,
            });

            let section_id = obj.section_id(StandardSection::Text);
            obj.add_symbol_data(symbol_id, section_id, &function.body, 1);
        }

        // Add function call trampolines
        for (signature_index, function) in function_call_trampolines.into_iter() {
            let function_name = metadata.get_function_call_trampoline_name(signature_index);
            let symbol_id = obj.add_symbol(Symbol {
                name: function_name.as_bytes().to_vec(),
                value: 0,
                size: 0,
                kind: SymbolKind::Text,
                scope: SymbolScope::Dynamic,
                weak: false,
                section: SymbolSection::Undefined,
                flags: SymbolFlags::None,
            });
            let section_id = obj.section_id(StandardSection::Text);
            obj.add_symbol_data(symbol_id, section_id, &function.body, 1);
        }

        // Add dynamic function trampolines
        for (func_index, function) in dynamic_function_trampolines.into_iter() {
            let function_name = metadata.get_dynamic_function_trampoline_name(func_index);
            let symbol_id = obj.add_symbol(Symbol {
                name: function_name.as_bytes().to_vec(),
                value: 0,
                size: 0,
                kind: SymbolKind::Text,
                scope: SymbolScope::Dynamic,
                weak: false,
                section: SymbolSection::Undefined,
                flags: SymbolFlags::None,
            });
            let section_id = obj.section_id(StandardSection::Text);
            obj.add_symbol_data(symbol_id, section_id, &function.body, 1);
        }

        // Add relocations (function and sections)
        let mut all_relocations = Vec::new();
        for (function_local_index, relocations) in function_relocations.into_iter() {
            let function_name = metadata.get_function_name(function_local_index);
            let symbol_id = obj.symbol_id(function_name.as_bytes()).unwrap();
            all_relocations.push((symbol_id, relocations))
        }
        for (section_index, relocations) in custom_section_relocations.into_iter() {
            let function_name = metadata.get_section_name(section_index);
            let symbol_id = obj.symbol_id(function_name.as_bytes()).unwrap();
            all_relocations.push((symbol_id, relocations))
        }
        for (symbol_id, relocations) in all_relocations.into_iter() {
            let (_symbol_id, section_offset) = obj.symbol_section_and_offset(symbol_id).unwrap();
            let section_id = obj.section_id(StandardSection::Text);
            for r in relocations {
                let relocation_address = section_offset + r.offset as u64;
                match r.reloc_target {
                    RelocationTarget::LocalFunc(index) => {
                        let target_name = metadata.get_function_name(index);
                        let target_symbol = obj.symbol_id(target_name.as_bytes()).unwrap();
                        obj.add_relocation(
                            section_id,
                            Relocation {
                                offset: relocation_address,
                                size: 32, // FIXME for all targets
                                kind: RelocationKind::PltRelative,
                                encoding: RelocationEncoding::X86Branch,
                                // size: 64, // FIXME for all targets
                                // kind: RelocationKind::Absolute,
                                // encoding: RelocationEncoding::Generic,
                                symbol: target_symbol,
                                addend: r.addend,
                            },
                        )
                        .map_err(to_compile_error)?;
                    }
                    RelocationTarget::LibCall(libcall) => {
                        let libcall_fn_name = libcall.to_function_name().as_bytes();
                        // We add the symols lazily as we see them
                        let target_symbol = obj.symbol_id(libcall_fn_name).unwrap_or_else(|| {
                            obj.add_symbol(Symbol {
                                name: libcall_fn_name.to_vec(),
                                value: 0,
                                size: 0,
                                kind: SymbolKind::Unknown,
                                scope: SymbolScope::Unknown,
                                weak: false,
                                section: SymbolSection::Undefined,
                                flags: SymbolFlags::None,
                            })
                        });
                        obj.add_relocation(
                            section_id,
                            Relocation {
                                offset: relocation_address,
                                size: 32, // FIXME for all targets
                                kind: RelocationKind::PltRelative,
                                encoding: RelocationEncoding::X86Branch,
                                // size: 64, // FIXME for all targets
                                // kind: RelocationKind::Absolute,
                                // encoding: RelocationEncoding::Generic,
                                symbol: target_symbol,
                                addend: r.addend,
                            },
                        )
                        .map_err(to_compile_error)?;
                    }
                    RelocationTarget::CustomSection(section_index) => {
                        let target_name = metadata.get_section_name(section_index);
                        let target_symbol = obj.symbol_id(target_name.as_bytes()).unwrap();
                        obj.add_relocation(
                            section_id,
                            Relocation {
                                offset: relocation_address,
                                size: 32, // FIXME for all targets
                                kind: RelocationKind::PltRelative,
                                encoding: RelocationEncoding::X86Branch,
                                // size: 64, // FIXME for all targets
                                // kind: RelocationKind::Absolute,
                                // encoding: RelocationEncoding::Generic,
                                symbol: target_symbol,
                                addend: r.addend,
                            },
                        )
                        .map_err(to_compile_error)?;
                    }
                    RelocationTarget::JumpTable(_func_index, _jt) => {
                        // do nothing
                    }
                };
            }
        }

        let filepath = {
            let file = tempfile::Builder::new()
                .prefix("wasmer_native")
                .suffix(".o")
                .tempfile()
                .map_err(to_compile_error)?;

            // Re-open it.
            let (mut file, filepath) = file.keep().map_err(to_compile_error)?;
            let obj_bytes = obj.write().map_err(to_compile_error)?;

            file.write(&obj_bytes).map_err(to_compile_error)?;
            filepath
        };

        let shared_filepath = {
            let suffix = format!(".{}", Self::get_default_extension(&target_triple));
            let shared_file = tempfile::Builder::new()
                .prefix("wasmer_native")
                .suffix(&suffix)
                .tempfile()
                .map_err(to_compile_error)?;
            shared_file
                .into_temp_path()
                .keep()
                .map_err(to_compile_error)?
        };

        let host_target = Triple::host();
        let is_cross_compiling = target_triple != host_target;
        let cross_compiling_args: Vec<String> = if is_cross_compiling {
            vec![
                format!("--target={}", target_triple),
                "-fuse-ld=lld".to_string(),
                "-nodefaultlibs".to_string(),
                "-nostdlib".to_string(),
            ]
        } else {
            vec![]
        };
        let target_args = match (target_triple.operating_system, is_cross_compiling) {
            (OperatingSystem::Windows, true) => vec!["-Wl,/force:unresolved"],
            (OperatingSystem::Windows, false) => vec!["-Wl,-undefined,dynamic_lookup"],
            _ => vec!["-nostartfiles", "-Wl,-undefined,dynamic_lookup"],
        };
        trace!(
            "Compiling for target {} from host {}",
            target_triple.to_string(),
            host_target.to_string()
        );

        let linker = if is_cross_compiling {
            "clang-10"
        } else {
            "gcc"
        };

        let output = Command::new(linker)
            .arg(&filepath)
            .arg("-o")
            .arg(&shared_filepath)
            .args(&target_args)
            // .args(&wasmer_symbols)
            .arg("-shared")
            .args(&cross_compiling_args)
            .arg("-v")
            .output()
            .map_err(to_compile_error)?;

        if !output.status.success() {
            return Err(CompileError::Codegen(format!(
                "Shared object file generator failed with:\nstderr:{}\nstdout:{}",
                String::from_utf8_lossy(&output.stderr).trim_end(),
                String::from_utf8_lossy(&output.stdout).trim_end()
            )));
        }
        trace!("gcc command result {:?}", output);
        if is_cross_compiling {
            Self::from_parts_crosscompiled(metadata, shared_filepath)
        } else {
            let lib = Library::new(&shared_filepath).map_err(to_compile_error)?;
            Self::from_parts(&mut engine_inner, metadata, shared_filepath, lib)
        }
    }

    /// Get the default extension when serializing this artifact
    pub fn get_default_extension(triple: &Triple) -> &'static str {
        match triple.operating_system {
            OperatingSystem::Windows => "dll",
            OperatingSystem::Darwin | OperatingSystem::Ios | OperatingSystem::MacOSX { .. } => {
                "dylib"
            }
            _ => "so",
        }
    }

    /// Construct a `NativeArtifact` from component parts.
    pub fn from_parts_crosscompiled(
        metadata: ModuleMetadata,
        sharedobject_path: PathBuf,
    ) -> Result<Self, CompileError> {
        let finished_functions: PrimaryMap<LocalFunctionIndex, *mut [VMFunctionBody]> =
            PrimaryMap::new();
        let finished_dynamic_function_trampolines: PrimaryMap<
            FunctionIndex,
            *mut [VMFunctionBody],
        > = PrimaryMap::new();
        let signatures: PrimaryMap<SignatureIndex, VMSharedSignatureIndex> = PrimaryMap::new();
        Ok(Self {
            sharedobject_path,
            metadata,
            library: None,
            finished_functions: finished_functions.into_boxed_slice(),
            finished_dynamic_function_trampolines: finished_dynamic_function_trampolines
                .into_boxed_slice(),
            signatures: signatures.into_boxed_slice(),
        })
    }

    /// Construct a `NativeArtifact` from component parts.
    pub fn from_parts(
        engine_inner: &mut NativeEngineInner,
        metadata: ModuleMetadata,
        sharedobject_path: PathBuf,
        lib: Library,
    ) -> Result<Self, CompileError> {
        let mut finished_functions: PrimaryMap<LocalFunctionIndex, *mut [VMFunctionBody]> =
            PrimaryMap::new();
        for (function_local_index, function_len) in metadata.function_body_lengths.iter() {
            let function_name = metadata.get_function_name(function_local_index);
            unsafe {
                // We use a fake function signature `fn()` because we just
                // want to get the function address.
                let func: LibrarySymbol<unsafe extern "C" fn()> = lib
                    .get(function_name.as_bytes())
                    .map_err(to_compile_error)?;
                let raw = *func.into_raw();
                // The function pointer is a fat pointer, however this information
                // is only used when retrieving the trap information which is not yet
                // implemented in this engine.
                let func_pointer =
                    std::slice::from_raw_parts(raw as *const (), *function_len as usize);
                let func_pointer = func_pointer as *const [()] as *mut [VMFunctionBody];
                finished_functions.push(func_pointer);
            }
        }

        // Retrieve function call trampolines (for all signatures in the module)
        for (sig_index, func_type) in metadata.compile_info.module.signatures.iter() {
            let function_name = metadata.get_function_call_trampoline_name(sig_index);
            unsafe {
                let trampoline: LibrarySymbol<VMTrampoline> = lib
                    .get(function_name.as_bytes())
                    .map_err(to_compile_error)?;
                engine_inner.add_trampoline(&func_type, *trampoline);
            }
        }

        // Retrieve dynamic function trampolines (only for imported functions)
        let mut finished_dynamic_function_trampolines: PrimaryMap<
            FunctionIndex,
            *mut [VMFunctionBody],
        > = PrimaryMap::with_capacity(metadata.compile_info.module.num_imported_funcs);
        for func_index in metadata
            .compile_info
            .module
            .functions
            .keys()
            .take(metadata.compile_info.module.num_imported_funcs)
        {
            let function_name = metadata.get_dynamic_function_trampoline_name(func_index);
            unsafe {
                let trampoline: LibrarySymbol<unsafe extern "C" fn()> = lib
                    .get(function_name.as_bytes())
                    .map_err(to_compile_error)?;
                let raw = *trampoline.into_raw();
                let trampoline_pointer = std::slice::from_raw_parts(raw as *const (), 0);
                let trampoline_pointer =
                    trampoline_pointer as *const [()] as *mut [VMFunctionBody];
                finished_dynamic_function_trampolines.push(trampoline_pointer);
            }
        }

        // Leaving frame infos from now, as they are not yet used
        // however they might be useful for the future.
        // let frame_infos = compilation
        //     .get_frame_info()
        //     .values()
        //     .map(|frame_info| SerializableFunctionFrameInfo::Processed(frame_info.clone()))
        //     .collect::<PrimaryMap<LocalFunctionIndex, _>>();
        // Self::from_parts(&mut engine_inner, lib, metadata, )
        // let frame_info_registration = register_frame_info(
        //     serializable.module.clone(),
        //     &finished_functions,
        //     serializable.compilation.function_frame_info.clone(),
        // );

        // Compute indices into the shared signature table.
        let signatures = {
            let signature_registry = engine_inner.signatures();
            metadata
                .compile_info
                .module
                .signatures
                .values()
                .map(|sig| signature_registry.register(sig))
                .collect::<PrimaryMap<_, _>>()
        };

        Ok(Self {
            sharedobject_path,
            metadata,
            library: Some(lib),
            finished_functions: finished_functions.into_boxed_slice(),
            finished_dynamic_function_trampolines: finished_dynamic_function_trampolines
                .into_boxed_slice(),
            signatures: signatures.into_boxed_slice(),
        })
    }

    /// Compile a data buffer into a `NativeArtifact`, which may then be instantiated.
    #[cfg(not(feature = "compiler"))]
    pub fn new(_engine: &NativeEngine, _data: &[u8]) -> Result<Self, CompileError> {
        Err(CompileError::Codegen(
            "Compilation is not enabled in the engine".to_string(),
        ))
    }

    /// Deserialize a `NativeArtifact` from bytes.
    ///
    /// # Safety
    ///
    /// The bytes must represent a serialized WebAssembly module.
    pub unsafe fn deserialize(
        engine: &NativeEngine,
        bytes: &[u8],
    ) -> Result<Self, DeserializeError> {
        if !Self::is_deserializable(&bytes) {
            return Err(DeserializeError::Incompatible(
                "The provided bytes are not in any native format Wasmer can understand".to_string(),
            ));
        }
        // Dump the bytes into a file, so we can read it with our `dlopen`
        let named_file = NamedTempFile::new()?;
        let (mut file, path) = named_file.keep().map_err(|e| e.error)?;
        file.write_all(&bytes)?;
        // We already checked for the header, so we don't need
        // to check again.
        Self::deserialize_from_file_unchecked(&engine, &path)
    }

    /// Deserialize a `NativeArtifact` from a file path.
    ///
    /// # Safety
    ///
    /// The file's content must represent a serialized WebAssembly module.
    pub unsafe fn deserialize_from_file(
        engine: &NativeEngine,
        path: &Path,
    ) -> Result<Self, DeserializeError> {
        let mut file = File::open(&path)?;
        let mut buffer = [0; 5];
        // read up to 5 bytes
        file.read_exact(&mut buffer)?;
        if !Self::is_deserializable(&buffer) {
            return Err(DeserializeError::Incompatible(
                "The provided bytes are not in any native format Wasmer can understand".to_string(),
            ));
        }
        Self::deserialize_from_file_unchecked(&engine, &path)
    }

    /// Deserialize a `NativeArtifact` from a file path (unchecked).
    ///
    /// # Safety
    ///
    /// The file's content must represent a serialized WebAssembly module.
    pub unsafe fn deserialize_from_file_unchecked(
        engine: &NativeEngine,
        path: &Path,
    ) -> Result<Self, DeserializeError> {
        let lib = Library::new(&path).map_err(|e| {
            DeserializeError::CorruptedBinary(format!("Library loading failed: {}", e))
        })?;
        let shared_path: PathBuf = PathBuf::from(path);
        // We use 10 + 1, as the length of the module will take 10 bytes
        // (we construct it like that in `metadata_length`) and we also want
        // to take the first element of the data to construct the slice from
        // it.
        let symbol: LibrarySymbol<*mut [u8; 10 + 1]> =
            lib.get(b"WASMER_METADATA").map_err(|e| {
                DeserializeError::CorruptedBinary(format!(
                    "The provided object file doesn't seem to be generated by Wasmer: {}",
                    e
                ))
            })?;
        use std::ops::Deref;
        use std::slice;

        let size = &mut **symbol.deref();
        let mut readable = &size[..];
        let metadata_len = leb128::read::unsigned(&mut readable).map_err(|_e| {
            DeserializeError::CorruptedBinary("Can't read metadata size".to_string())
        })?;
        let metadata_slice: &'static [u8] =
            slice::from_raw_parts(&size[10] as *const u8, metadata_len as usize);
        let metadata: ModuleMetadata = bincode::deserialize(metadata_slice)
            .map_err(|e| DeserializeError::CorruptedBinary(format!("{:?}", e)))?;
        let mut engine_inner = engine.inner_mut();

        Self::from_parts(&mut engine_inner, metadata, shared_path, lib)
            .map_err(DeserializeError::Compiler)
    }
}

impl Artifact for NativeArtifact {
    fn module(&self) -> Arc<ModuleInfo> {
        self.metadata.compile_info.module.clone()
    }

    fn module_ref(&self) -> &ModuleInfo {
        &self.metadata.compile_info.module
    }

    fn module_mut(&mut self) -> Option<&mut ModuleInfo> {
        Arc::get_mut(&mut self.metadata.compile_info.module)
    }

    fn register_frame_info(&self) {
        // Do nothing for now
    }

    fn features(&self) -> &Features {
        &self.metadata.compile_info.features
    }

    fn data_initializers(&self) -> &[OwnedDataInitializer] {
        &*self.metadata.data_initializers
    }

    fn memory_plans(&self) -> &PrimaryMap<MemoryIndex, MemoryPlan> {
        &self.metadata.compile_info.memory_plans
    }

    fn table_plans(&self) -> &PrimaryMap<TableIndex, TablePlan> {
        &self.metadata.compile_info.table_plans
    }

    fn finished_functions(&self) -> &BoxedSlice<LocalFunctionIndex, *mut [VMFunctionBody]> {
        &self.finished_functions
    }

    fn finished_dynamic_function_trampolines(
        &self,
    ) -> &BoxedSlice<FunctionIndex, *mut [VMFunctionBody]> {
        &self.finished_dynamic_function_trampolines
    }

    fn signatures(&self) -> &BoxedSlice<SignatureIndex, VMSharedSignatureIndex> {
        &self.signatures
    }

    fn preinstantiate(&self) -> Result<(), InstantiationError> {
        if self.library.is_none() {
            return Err(InstantiationError::Link(LinkError::Trap(
                RuntimeError::new("Cross compiled artifacts can't be instantiated."),
            )));
        }
        Ok(())
    }

    /// Serialize a NativeArtifact
    fn serialize(&self) -> Result<Vec<u8>, SerializeError> {
        Ok(std::fs::read(&self.sharedobject_path)?)
    }
}
