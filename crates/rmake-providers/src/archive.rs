//! Read-only validation for package-local prebuilt static archives.

use std::{fs, path::Component, path::Path};

#[cfg(target_os = "macos")]
use object::read::macho::LoadCommandIterator;
use object::{
    Architecture, BinaryFormat, Endianness, Object, ObjectKind, read::archive::ArchiveFile,
};
use rmake_model::{
    ArchiveEvidence, Result, StaticFolderImport, StaticFolderRequest, StaticFolderResolver,
};

use crate::{LocalFolderProvider, ensure_contained, folder_error, io_diagnostic};

impl StaticFolderResolver for LocalFolderProvider {
    fn resolve_static(
        &self,
        package_root: &Path,
        request: &StaticFolderRequest,
    ) -> Result<StaticFolderImport> {
        let mut header_request = request.headers.clone();
        let empty_includes = header_request.includes.as_ref().is_some_and(Vec::is_empty);
        if empty_includes {
            // Reuse header path, alias, definition, and standard validation, then
            // remove include usage for archives that intentionally export none.
            header_request.includes = Some(vec![".".into()]);
        }
        let mut headers =
            crate::HeaderFolderResolver::resolve(self, package_root, &header_request)?;
        if empty_includes {
            headers.usage.include_dirs.clear();
        }

        let package_root = fs::canonicalize(package_root)
            .map_err(|error| io_diagnostic("cannot resolve package root", package_root, error))?;
        let library = resolve_library(&package_root, &headers.root, &request.library)?;
        let evidence = inspect_archive(&library)?;

        Ok(StaticFolderImport {
            headers,
            library,
            language: request.language,
            compatibility: request.compatibility,
            evidence,
        })
    }
}

fn resolve_library(
    package_root: &Path,
    folder_root: &Path,
    requested: &Path,
) -> Result<std::path::PathBuf> {
    if requested.as_os_str().is_empty() || requested.is_absolute() {
        return Err(folder_error(
            "RMAKE-F201",
            format!("static library path {requested:?} must be a non-empty folder-relative path"),
        ));
    }
    let mut current = folder_root.to_path_buf();
    for component in requested.components() {
        match component {
            Component::CurDir => continue,
            Component::Normal(part) => current.push(part),
            _ => {
                return Err(folder_error(
                    "RMAKE-F201",
                    format!(
                        "static library path {requested:?} must contain only folder-relative components"
                    ),
                ));
            }
        }
        let metadata = fs::symlink_metadata(&current).map_err(|error| {
            io_diagnostic("cannot inspect static library path", &current, error)
        })?;
        if metadata.file_type().is_symlink() {
            return Err(folder_error(
                "RMAKE-F203",
                format!(
                    "static library path {requested:?} contains symlink component {}",
                    current.display()
                ),
            ));
        }
    }

    let canonical = fs::canonicalize(&current)
        .map_err(|error| io_diagnostic("cannot resolve static library", &current, error))?;
    let metadata = fs::metadata(&canonical)
        .map_err(|error| io_diagnostic("cannot inspect static library", &canonical, error))?;
    if !metadata.is_file() {
        return Err(folder_error(
            "RMAKE-F202",
            format!("static library {:?} is not a regular file", requested),
        ));
    }
    ensure_contained(&canonical, folder_root, "static library", requested)?;
    ensure_contained(&canonical, package_root, "static library", requested)?;
    if canonical.starts_with(package_root.join("build/rmake")) {
        return Err(folder_error(
            "RMAKE-F206",
            format!(
                "static library inputs must be outside RMake-owned build/rmake: {}",
                canonical.display()
            ),
        ));
    }
    Ok(canonical)
}

fn inspect_archive(path: &Path) -> Result<ArchiveEvidence> {
    let bytes =
        fs::read(path).map_err(|error| io_diagnostic("cannot read static archive", path, error))?;
    let archive = ArchiveFile::parse(bytes.as_slice()).map_err(|error| {
        folder_error(
            "RMAKE-F207",
            format!("invalid static archive {}: {error}", path.display()),
        )
    })?;
    if archive.is_thin() {
        return Err(folder_error(
            "RMAKE-F207",
            format!("thin archives are unsupported: {}", path.display()),
        ));
    }
    if !matches!(
        archive.kind(),
        object::read::archive::ArchiveKind::Unknown
            | object::read::archive::ArchiveKind::Gnu
            | object::read::archive::ArchiveKind::Gnu64
            | object::read::archive::ArchiveKind::Bsd
            | object::read::archive::ArchiveKind::Bsd64
    ) {
        return Err(folder_error(
            "RMAKE-F207",
            format!("unsupported archive dialect in {}", path.display()),
        ));
    }

    let native_format = native_format().ok_or_else(|| {
        folder_error(
            "RMAKE-F208",
            "static archive validation supports Linux ELF and macOS Mach-O hosts",
        )
    })?;
    let native_architecture = native_architecture().ok_or_else(|| {
        folder_error(
            "RMAKE-F208",
            format!("unsupported host architecture {:?}", std::env::consts::ARCH),
        )
    })?;
    let native_endianness = if cfg!(target_endian = "little") {
        Endianness::Little
    } else {
        Endianness::Big
    };
    let mut members = 0;

    for member in archive.members() {
        let member = member.map_err(|error| {
            folder_error(
                "RMAKE-F207",
                format!("malformed member in {}: {error}", path.display()),
            )
        })?;
        let name = String::from_utf8_lossy(member.name());
        if member.is_thin() {
            return Err(folder_error(
                "RMAKE-F207",
                format!(
                    "thin archive member {name:?} in {} is unsupported",
                    path.display()
                ),
            ));
        }
        let data = member.data(bytes.as_slice()).map_err(|error| {
            folder_error(
                "RMAKE-F207",
                format!(
                    "truncated archive member {name:?} in {}: {error}",
                    path.display()
                ),
            )
        })?;
        let file = object::File::parse(data).map_err(|error| {
            folder_error(
                "RMAKE-F207",
                format!(
                    "archive member {name:?} in {} is not a supported object file: {error}",
                    path.display()
                ),
            )
        })?;
        if file.kind() != ObjectKind::Relocatable {
            return Err(folder_error(
                "RMAKE-F207",
                format!(
                    "archive member {name:?} in {} is not a relocatable object",
                    path.display()
                ),
            ));
        }
        if file.format() != native_format {
            return Err(folder_error(
                "RMAKE-F208",
                format!(
                    "archive member {name:?} has foreign format {:?}; host requires {:?}",
                    file.format(),
                    native_format
                ),
            ));
        }
        if file.architecture() != native_architecture {
            return Err(folder_error(
                "RMAKE-F208",
                format!(
                    "archive member {name:?} has foreign architecture {:?}; host requires {:?}",
                    file.architecture(),
                    native_architecture
                ),
            ));
        }
        if file.is_64() != (usize::BITS == 64) {
            return Err(folder_error(
                "RMAKE-F208",
                format!("archive member {name:?} has incompatible pointer width for this host"),
            ));
        }
        if file.endianness() != native_endianness {
            return Err(folder_error(
                "RMAKE-F208",
                format!("archive member {name:?} has foreign byte order"),
            ));
        }
        validate_platform(&file, path, &name)?;
        members += 1;
    }

    if members == 0 {
        return Err(folder_error(
            "RMAKE-F207",
            format!(
                "static archive {} contains no object members",
                path.display()
            ),
        ));
    }

    Ok(ArchiveEvidence {
        format: format!("{native_format:?}"),
        architecture: format!("{native_architecture:?}"),
        members,
    })
}

fn validate_platform(file: &object::File<'_>, path: &Path, name: &str) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        use object::elf::{ELFOSABI_LINUX, ELFOSABI_NONE, ELFOSABI_SYSV};
        let os_abi = match file {
            object::File::Elf32(file) => file.elf_header().e_ident.os_abi,
            object::File::Elf64(file) => file.elf_header().e_ident.os_abi,
            _ => unreachable!("native Linux format was already checked"),
        };
        if os_abi != ELFOSABI_NONE && os_abi != ELFOSABI_SYSV && os_abi != ELFOSABI_LINUX {
            return Err(folder_error(
                "RMAKE-F208",
                format!(
                    "archive member {name:?} targets non-Linux ELF ABI {os_abi:?} in {}",
                    path.display()
                ),
            ));
        }
    }
    #[cfg(target_os = "macos")]
    {
        match file {
            object::File::MachO32(file) => {
                let commands = file.macho_load_commands().map_err(|error| {
                    folder_error(
                        "RMAKE-F207",
                        format!(
                            "malformed Mach-O commands in member {name:?} of {}: {error}",
                            path.display()
                        ),
                    )
                })?;
                validate_macho_platform(commands, file.endian(), path, name)?;
            }
            object::File::MachO64(file) => {
                let commands = file.macho_load_commands().map_err(|error| {
                    folder_error(
                        "RMAKE-F207",
                        format!(
                            "malformed Mach-O commands in member {name:?} of {}: {error}",
                            path.display()
                        ),
                    )
                })?;
                validate_macho_platform(commands, file.endian(), path, name)?;
            }
            _ => unreachable!("native macOS format was already checked"),
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let _ = (file, path, name);
    Ok(())
}

#[cfg(target_os = "macos")]
fn validate_macho_platform<E: object::Endian>(
    mut commands: LoadCommandIterator<'_, E>,
    endian: E,
    path: &Path,
    name: &str,
) -> Result<()> {
    loop {
        let command = commands.next().map_err(|error| {
            folder_error(
                "RMAKE-F207",
                format!(
                    "malformed Mach-O command in member {name:?} of {}: {error}",
                    path.display()
                ),
            )
        })?;
        let Some(command) = command else { break };
        let legacy_platform = match command.cmd() {
            object::macho::LC_VERSION_MIN_MACOSX => Some(true),
            object::macho::LC_VERSION_MIN_IPHONEOS
            | object::macho::LC_VERSION_MIN_TVOS
            | object::macho::LC_VERSION_MIN_WATCHOS => Some(false),
            _ => None,
        };
        if legacy_platform == Some(false) {
            return Err(folder_error(
                "RMAKE-F208",
                format!(
                    "archive member {name:?} targets a non-macOS Apple platform in {}",
                    path.display()
                ),
            ));
        }
        if command.cmd() == object::macho::LC_BUILD_VERSION {
            let build = command
                .build_version(endian)
                .map_err(|error| {
                    folder_error(
                        "RMAKE-F207",
                        format!(
                            "malformed Mach-O build version in member {name:?} of {}: {error}",
                            path.display()
                        ),
                    )
                })?
                .ok_or_else(|| {
                    folder_error(
                        "RMAKE-F207",
                        format!("missing Mach-O build version in member {name:?}"),
                    )
                })?
                .0;
            if build.platform.get(endian) != object::macho::PLATFORM_MACOS {
                return Err(folder_error(
                    "RMAKE-F208",
                    format!(
                        "archive member {name:?} targets non-macOS Apple platform {} in {}",
                        build.platform.get(endian),
                        path.display()
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn native_format() -> Option<BinaryFormat> {
    if cfg!(target_os = "macos") {
        Some(BinaryFormat::MachO)
    } else if cfg!(target_os = "linux") {
        Some(BinaryFormat::Elf)
    } else {
        None
    }
}

fn native_architecture() -> Option<Architecture> {
    Some(match std::env::consts::ARCH {
        "aarch64" => Architecture::Aarch64,
        "x86_64" => Architecture::X86_64,
        "x86" => Architecture::I386,
        "arm" => Architecture::Arm,
        "mips" => Architecture::Mips,
        "mips64" => Architecture::Mips64,
        "powerpc" => Architecture::PowerPc,
        "powerpc64" => Architecture::PowerPc64,
        "riscv32" => Architecture::Riscv32,
        "riscv64" => Architecture::Riscv64,
        "s390x" => Architecture::S390x,
        "sparc" => Architecture::Sparc,
        "sparc64" => Architecture::Sparc64,
        "loongarch32" => Architecture::LoongArch32,
        "loongarch64" => Architecture::LoongArch64,
        _ => return None,
    })
}
