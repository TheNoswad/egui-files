//! What a program or library is: its format and architecture, how it's
//! linked, and for Windows ones the version details they carry. Read in the
//! background, for any file that starts like an executable whatever its name.

use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use object::{
    Architecture, Endianness, FileKind, LittleEndian as LE, Object, ReadCache, ReadRef,
    SubArchitecture,
    macho::{self, FatArch32, FatArch64},
    pe::{self, ImageNtHeaders32, ImageNtHeaders64},
    read::{
        elf::{ElfFile, FileHeader, ProgramHeader},
        macho::{FatArch, MachHeader, MachOFatFile, MachOFile},
        pe::{
            ImageNtHeaders, ImageOptionalHeader, PeFile, ResourceDirectoryEntryData,
            ResourceNameOrId,
        },
    },
};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Program {
    /// Format, architecture, kind… in words.
    pub rows: Vec<(String, String)>,
    /// The libraries it loads, in the order it names them.
    pub libraries: Vec<String>,
}

pub struct ProgramProbe {
    pub path: PathBuf,
    /// `None` once read: not a program.
    cell: Arc<OnceLock<Result<Option<Program>, String>>>,
}

impl ProgramProbe {
    pub fn start(ctx: &egui::Context, path: PathBuf) -> Self {
        let cell = Arc::new(OnceLock::new());
        let (c, ctx, p) = (cell.clone(), ctx.clone(), path.clone());
        std::thread::Builder::new()
            .name("properties-program".into())
            .spawn(move || {
                let _ = c.set(read(&p));
                ctx.request_repaint();
            })
            .expect("spawn thread");
        Self { path, cell }
    }

    pub fn get(&self) -> Option<&Result<Option<Program>, String>> {
        self.cell.get()
    }
}

/// Whether the first bytes are those of a program or library this reads.
fn looks_like_program(head: &[u8]) -> bool {
    match head {
        [b'M', b'Z', ..] | [0x7f, b'E', b'L', b'F', ..] => true,
        [0xfe, 0xed, 0xfa, 0xce | 0xcf, ..] | [0xce | 0xcf, 0xfa, 0xed, 0xfe, ..] => true,
        // Universal Mach-O, or a Java class: those count versions from 45
        // where this counts architectures.
        [0xca, 0xfe, 0xba, 0xbe | 0xbf, a, b, c, d, ..] => {
            (1..=20).contains(&u32::from_be_bytes([*a, *b, *c, *d]))
        }
        _ => false,
    }
}

pub fn read(path: &Path) -> Result<Option<Program>, String> {
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let mut head = [0u8; 8];
    let n = file.read(&mut head).map_err(|e| e.to_string())?;
    if !looks_like_program(&head[..n]) {
        return Ok(None);
    }
    let cache = ReadCache::new(file);
    let data = &cache;
    let kind = match FileKind::parse(data) {
        Ok(kind) => kind,
        // "MZ" alone: an MS-DOS program, or not a program at all.
        Err(_) if head.starts_with(b"MZ") => {
            return Ok(Some(Program {
                rows: vec![("Format".into(), "MS-DOS program".into())],
                libraries: Vec::new(),
            }));
        }
        Err(e) => return Err(e.to_string()),
    };
    let program = match kind {
        FileKind::Pe32 => pe_file::<ImageNtHeaders32, _>(data),
        FileKind::Pe64 => pe_file::<ImageNtHeaders64, _>(data),
        FileKind::Elf32 => elf_file::<object::elf::FileHeader32<Endianness>, _>(data),
        FileKind::Elf64 => elf_file::<object::elf::FileHeader64<Endianness>, _>(data),
        FileKind::MachO32 => macho_file::<macho::MachHeader32<Endianness>, _>(data),
        FileKind::MachO64 => macho_file::<macho::MachHeader64<Endianness>, _>(data),
        FileKind::MachOFat32 => fat_file::<FatArch32, _>(data),
        FileKind::MachOFat64 => fat_file::<FatArch64, _>(data),
        _ => return Ok(None),
    };
    program.map(Some).map_err(|e| e.to_string())
}

type Result2<T> = object::Result<T>;

fn arch_name(arch: Architecture, sub: Option<SubArchitecture>) -> String {
    if sub == Some(SubArchitecture::Arm64EC) {
        return "ARM64EC".into();
    }
    match arch {
        Architecture::X86_64 => "x86-64",
        Architecture::X86_64_X32 => "x86-64 (x32)",
        Architecture::I386 => "x86 (32-bit)",
        Architecture::Aarch64 => "ARM64",
        Architecture::Aarch64_Ilp32 => "ARM64 (ILP32)",
        Architecture::Arm => "ARM (32-bit)",
        Architecture::Riscv64 => "RISC-V (64-bit)",
        Architecture::Riscv32 => "RISC-V (32-bit)",
        Architecture::PowerPc64 => "PowerPC (64-bit)",
        Architecture::PowerPc => "PowerPC (32-bit)",
        Architecture::Mips64 => "MIPS (64-bit)",
        Architecture::Mips => "MIPS (32-bit)",
        Architecture::LoongArch64 => "LoongArch (64-bit)",
        Architecture::S390x => "IBM Z",
        Architecture::Sparc64 => "SPARC (64-bit)",
        Architecture::Unknown => "Unknown",
        other => return format!("{other:?}"),
    }
    .into()
}

fn row(rows: &mut Vec<(String, String)>, label: &str, value: impl Into<String>) {
    rows.push((label.into(), value.into()));
}

// ---------------------------------------------------------------------------
// Windows

fn pe_file<'data, Pe: ImageNtHeaders, R: ReadRef<'data>>(data: R) -> Result2<Program> {
    let file = PeFile::<Pe, R>::parse(data)?;
    let mut rows = Vec::new();
    let bits = if file.is_64() { "64" } else { "32" };
    row(&mut rows, "Format", format!("Windows PE ({bits}-bit)"));
    let dotnet = dotnet_flags(&file, data);
    // .NET code alone, not held to 32-bit, runs as whatever the machine is.
    let any_cpu = dotnet.is_some_and(|flags| {
        flags & pe::COMIMAGE_FLAGS_ILONLY != 0 && flags & pe::COMIMAGE_FLAGS_32BITREQUIRED == 0
    }) && file.architecture() == Architecture::I386;
    row(
        &mut rows,
        "Architecture",
        if any_cpu {
            "Any (.NET)".to_string()
        } else {
            arch_name(file.architecture(), file.sub_architecture())
        },
    );
    let characteristics = file.nt_headers().file_header().characteristics.get(LE);
    let subsystem = file.nt_headers().optional_header().subsystem();
    let kind = if characteristics & pe::IMAGE_FILE_DLL != 0 {
        "Library (DLL)"
    } else {
        match subsystem {
            pe::IMAGE_SUBSYSTEM_NATIVE => "Driver",
            pe::IMAGE_SUBSYSTEM_EFI_APPLICATION => "UEFI program",
            pe::IMAGE_SUBSYSTEM_EFI_BOOT_SERVICE_DRIVER
            | pe::IMAGE_SUBSYSTEM_EFI_RUNTIME_DRIVER => "UEFI driver",
            _ => "Program",
        }
    };
    row(&mut rows, "Kind", kind);
    let ui = match subsystem {
        pe::IMAGE_SUBSYSTEM_WINDOWS_GUI => Some("Windows (graphical)"),
        pe::IMAGE_SUBSYSTEM_WINDOWS_CUI => Some("Console"),
        _ => None,
    };
    if let Some(ui) = ui
        && kind == "Program"
    {
        row(&mut rows, "Runs in", ui);
    }
    if dotnet.is_some() {
        row(&mut rows, "Runtime", ".NET");
    }
    let present = |id: usize| file.data_directory(id).is_some_and(|d| d.size.get(LE) > 0);
    row(
        &mut rows,
        "Signature",
        if present(pe::IMAGE_DIRECTORY_ENTRY_SECURITY) {
            "Signed (not checked)"
        } else {
            "Not signed"
        },
    );
    if let Some(info) = version_resource(&file, data) {
        rows.extend(version_rows(info));
    }
    let mut libraries: Vec<String> = Vec::new();
    for import in file.imports().unwrap_or_default() {
        let name = String::from_utf8_lossy(import.library()).into_owned();
        if !libraries.iter().any(|l| l.eq_ignore_ascii_case(&name)) {
            libraries.push(name);
        }
    }
    Ok(Program { rows, libraries })
}

/// The flags of a .NET assembly's CLR header; `None` when it isn't one.
fn dotnet_flags<'data, Pe: ImageNtHeaders, R: ReadRef<'data>>(
    file: &PeFile<'data, Pe, R>,
    data: R,
) -> Option<u32> {
    let dir = file.data_directory(pe::IMAGE_DIRECTORY_ENTRY_COM_DESCRIPTOR)?;
    if dir.size.get(LE) == 0 {
        return None;
    }
    let header = file
        .section_table()
        .pe_data_at(data, dir.virtual_address.get(LE))?;
    // IMAGE_COR20_HEADER: cb, runtime versions, metadata, then Flags.
    Some(le32(header, 16).unwrap_or(0))
}

/// The raw `VS_VERSIONINFO`, from the first language it's in.
fn version_resource<'data, Pe: ImageNtHeaders, R: ReadRef<'data>>(
    file: &PeFile<'data, Pe, R>,
    data: R,
) -> Option<&'data [u8]> {
    let sections = file.section_table();
    let dir = file
        .data_directories()
        .resource_directory(data, &sections)
        .ok()??;
    let root = dir.root().ok()?;
    let by_type = root
        .entries
        .iter()
        .find(|e| matches!(e.name_or_id(), ResourceNameOrId::Id(id) if id == pe::RT_VERSION))?;
    let ResourceDirectoryEntryData::Table(by_name) = by_type.data(dir).ok()? else {
        return None;
    };
    let ResourceDirectoryEntryData::Table(by_language) = by_name.entries.first()?.data(dir).ok()?
    else {
        return None;
    };
    let ResourceDirectoryEntryData::Data(entry) = by_language.entries.first()?.data(dir).ok()?
    else {
        return None;
    };
    let bytes = sections.pe_data_at(data, entry.offset_to_data.get(LE))?;
    bytes.get(..entry.size.get(LE) as usize)
}

/// One node of a version resource: a key, a value, and nodes inside.
struct Block<'a> {
    key: String,
    value: &'a [u8],
    text: bool,
    children: &'a [u8],
}

fn le16(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(data.get(at..at + 2)?.try_into().ok()?))
}

fn le32(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

fn align4(n: usize) -> usize {
    (n + 3) & !3
}

/// UTF-16 up to its terminating zero.
fn utf16(data: &[u8]) -> String {
    let units: Vec<u16> = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&c| u16::from_le_bytes(c))
        .take_while(|&u| u != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

/// The blocks one after another in `data`, each starting 4-byte aligned.
fn blocks(data: &[u8]) -> Vec<Block<'_>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at + 6 <= data.len() {
        let Some(block) = block(data, at) else { break };
        let len = le16(data, at).unwrap_or(0) as usize;
        out.push(block);
        at = align4(at + len);
    }
    out
}

fn block(data: &[u8], at: usize) -> Option<Block<'_>> {
    let len = le16(data, at)? as usize;
    if len < 6 {
        return None;
    }
    let all = data.get(at..at + len)?;
    let value_len = le16(all, 2)? as usize;
    let text = le16(all, 4)? == 1;
    let mut pos = 6;
    let key_units = all[pos..]
        .as_chunks::<2>()
        .0
        .iter()
        .position(|c| c == &[0, 0])
        .unwrap_or((all.len() - pos) / 2);
    let key = utf16(&all[pos..pos + key_units * 2]);
    pos = align4(pos + key_units * 2 + 2);
    // Text values count UTF-16 units, others bytes.
    let value_bytes = if text { value_len * 2 } else { value_len };
    let value_start = pos.min(all.len());
    let value_end = (pos + value_bytes).min(all.len());
    let value = &all[value_start..value_end];
    let children = &all[align4(value_end).min(all.len())..];
    Some(Block {
        key,
        value,
        text,
        children,
    })
}

/// The version details worth showing, in words, from a `VS_VERSIONINFO`.
fn version_rows(info: &[u8]) -> Vec<(String, String)> {
    let Some(root) = block(info, 0).filter(|b| b.key == "VS_VERSION_INFO") else {
        return Vec::new();
    };
    // VS_FIXEDFILEINFO: the version as numbers, for when there's no text.
    let fixed = (le32(root.value, 0) == Some(0xFEEF_04BD))
        .then(|| {
            let part = |at| le32(root.value, at);
            let (ms, ls) = (part(8)?, part(12)?);
            Some(format!(
                "{}.{}.{}.{}",
                ms >> 16,
                ms & 0xffff,
                ls >> 16,
                ls & 0xffff
            ))
        })
        .flatten();
    let mut strings: Vec<(String, String)> = Vec::new();
    for info in blocks(root.children) {
        if info.key != "StringFileInfo" {
            continue;
        }
        // The first table (language) there is.
        if let Some(table) = blocks(info.children).into_iter().next() {
            for s in blocks(table.children) {
                let value = if s.text {
                    utf16(s.value)
                } else {
                    String::from_utf8_lossy(s.value).into_owned()
                };
                let value = value.trim().to_string();
                if !value.is_empty() {
                    strings.push((s.key, value));
                }
            }
        }
    }
    let take = |key: &str| {
        strings
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    };
    let mut rows = Vec::new();
    const NAMED: &[(&str, &str)] = &[
        ("FileDescription", "Description"),
        ("ProductName", "Product"),
        ("CompanyName", "Company"),
        ("FileVersion", "File version"),
        ("ProductVersion", "Product version"),
        ("LegalCopyright", "Copyright"),
        ("OriginalFilename", "Original name"),
        ("Comments", "Comments"),
        ("LegalTrademarks", "Trademarks"),
    ];
    for &(key, label) in NAMED {
        let value = match take(key) {
            Some(value) => Some(value),
            None if key == "FileVersion" => fixed.clone(),
            None => None,
        };
        if let Some(value) = value {
            rows.push((label.to_string(), value));
        }
    }
    rows
}

// ---------------------------------------------------------------------------
// Linux and the BSDs

fn elf_file<'data, Elf, R>(data: R) -> Result2<Program>
where
    Elf: FileHeader<Endian = Endianness>,
    R: ReadRef<'data>,
{
    let file = ElfFile::<Elf, R>::parse(data)?;
    let endian = file.endian();
    let header = file.elf_header();
    let mut rows = Vec::new();
    let bits = if file.is_64() { "64" } else { "32" };
    let order = if endian == Endianness::Big {
        ", big-endian"
    } else {
        ""
    };
    row(&mut rows, "Format", format!("ELF ({bits}-bit{order})"));
    row(
        &mut rows,
        "Architecture",
        arch_name(file.architecture(), file.sub_architecture()),
    );

    let interpreter = file
        .elf_program_headers()
        .iter()
        .find_map(|ph| ph.interpreter(endian, data).ok().flatten())
        .map(|i| String::from_utf8_lossy(i).into_owned());
    let mut libraries = Vec::new();
    let mut soname = None;
    if let Ok(table) = file.elf_section_table().dynamic_table(endian, data) {
        for d in &table {
            let name = || {
                table
                    .string(d)
                    .ok()
                    .map(|s| String::from_utf8_lossy(s).into_owned())
            };
            match d.tag {
                object::elf::DT_NEEDED => libraries.extend(name()),
                object::elf::DT_SONAME => soname = name(),
                _ => {}
            }
        }
    }

    let kind = match header.e_type(endian) {
        object::elf::ET_EXEC => "Program",
        // A library by name, even one that runs too (libc does).
        object::elf::ET_DYN if soname.is_some() => "Library",
        // Position-independent programs are shared objects with an
        // interpreter to start them.
        object::elf::ET_DYN if interpreter.is_some() => "Program",
        object::elf::ET_DYN if header.e_entry(endian).into() == 0 => "Library",
        object::elf::ET_DYN => "Program",
        object::elf::ET_REL => "Object file",
        object::elf::ET_CORE => "Core dump",
        _ => "Unknown",
    };
    row(&mut rows, "Kind", kind);
    if let Some(soname) = soname {
        row(&mut rows, "Library name", soname);
    }
    if kind == "Program" {
        let linked = match &interpreter {
            Some(interpreter) => format!("Dynamically, by {interpreter}"),
            None if libraries.is_empty() => "Statically".to_string(),
            None => "Dynamically".to_string(),
        };
        row(&mut rows, "Linked", linked);
    }
    let symbols = if file.has_debug_symbols() {
        "Debug info"
    } else if file.section_by_name(".symtab").is_some() {
        "Symbol table"
    } else {
        "Stripped"
    };
    row(&mut rows, "Symbols", symbols);
    if let Ok(Some(id)) = file.build_id() {
        row(&mut rows, "Build ID", hex(id));
    }
    Ok(Program { rows, libraries })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// macOS

fn macho_file<'data, Mach, R>(data: R) -> Result2<Program>
where
    Mach: MachHeader<Endian = Endianness>,
    R: ReadRef<'data>,
{
    let file = MachOFile::<Mach, R>::parse(data)?;
    let endian = file.endian();
    let mut rows = Vec::new();
    let bits = if file.is_64() { "64" } else { "32" };
    row(&mut rows, "Format", format!("Mach-O ({bits}-bit)"));
    row(
        &mut rows,
        "Architecture",
        arch_name(file.architecture(), file.sub_architecture()),
    );
    let kind = match file.macho_header().filetype(endian) {
        macho::MH_EXECUTE => "Program",
        macho::MH_DYLIB => "Library",
        macho::MH_BUNDLE => "Plug-in",
        macho::MH_OBJECT => "Object file",
        macho::MH_CORE => "Core dump",
        macho::MH_KEXT_BUNDLE => "Kernel extension",
        _ => "Unknown",
    };
    row(&mut rows, "Kind", kind);
    let mut libraries = Vec::new();
    let mut signed = false;
    if let Ok(mut commands) = file.macho_load_commands() {
        while let Ok(Some(command)) = commands.next() {
            if command.cmd() == macho::LC_CODE_SIGNATURE {
                signed = true;
            }
            if let Ok(Some(dylib)) = command.dylib()
                && let Ok(name) = command.string(endian, dylib.dylib.name)
            {
                libraries.push(String::from_utf8_lossy(name).into_owned());
            }
        }
    }
    row(
        &mut rows,
        "Signature",
        if signed {
            "Signed (not checked)"
        } else {
            "Not signed"
        },
    );
    Ok(Program { rows, libraries })
}

/// A universal binary: the architectures in it, and the first one's details.
fn fat_file<'data, Fat: FatArch, R: ReadRef<'data>>(data: R) -> Result2<Program> {
    let fat = MachOFatFile::<Fat>::parse(data)?;
    let arches: Vec<String> = fat
        .arches()
        .iter()
        .map(|a| arch_name(a.architecture(), None))
        .collect();
    let mut program = match fat.arches().first() {
        Some(first) => {
            let slice = first.data(data)?;
            match FileKind::parse(slice)? {
                FileKind::MachO32 => macho_file::<macho::MachHeader32<Endianness>, _>(slice)?,
                FileKind::MachO64 => macho_file::<macho::MachHeader64<Endianness>, _>(slice)?,
                _ => Program::default(),
            }
        }
        None => Program::default(),
    };
    program
        .rows
        .retain(|(label, _)| label != "Format" && label != "Architecture");
    program
        .rows
        .insert(0, ("Format".into(), "Mach-O universal".into()));
    program
        .rows
        .insert(1, ("Architectures".into(), arches.join(", ")));
    Ok(program)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_this_test_binary() {
        let exe = std::env::current_exe().unwrap();
        let program = read(&exe).unwrap().expect("a program");
        let get = |label: &str| {
            program
                .rows
                .iter()
                .find(|(l, _)| l == label)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("Kind"), Some("Program"), "{program:?}");
        let arch = get("Architecture").unwrap();
        if cfg!(target_arch = "x86_64") {
            assert_eq!(arch, "x86-64");
        }
        if cfg!(windows) {
            assert!(get("Format").unwrap().starts_with("Windows PE"));
            assert!(
                program
                    .libraries
                    .iter()
                    .any(|l| l.eq_ignore_ascii_case("kernel32.dll"))
            );
        } else if cfg!(target_os = "linux") {
            assert!(get("Format").unwrap().starts_with("ELF"));
            assert!(get("Linked").unwrap().starts_with("Dynamically"));
            assert!(program.libraries.iter().any(|l| l.starts_with("libc.so")));
        }
    }

    #[test]
    fn other_files_are_not_programs() {
        let dir = std::env::temp_dir().join(format!("egui-files-program-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let text = dir.join("note.txt");
        std::fs::write(&text, "MZ is not enough? it is: but this fails to parse").unwrap();
        // Starts with "MZ" but isn't PE: shown as an MS-DOS program.
        let dos = read(&text).unwrap().unwrap();
        assert_eq!(dos.rows[0].1, "MS-DOS program");
        let plain = dir.join("plain.txt");
        std::fs::write(&plain, "hello").unwrap();
        assert_eq!(read(&plain).unwrap(), None);
        // A Java class: same magic as a universal binary.
        let class = dir.join("A.class");
        std::fs::write(&class, [0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 61, 0, 0]).unwrap();
        assert_eq!(read(&class).unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A hand-built version resource, as Windows' resource compiler lays it out.
    #[test]
    fn reads_version_details() {
        fn block(key: &str, value: &[u8], text: bool, children: &[Vec<u8>]) -> Vec<u8> {
            let mut b = vec![0u8; 6];
            for u in key.encode_utf16().chain([0]) {
                b.extend(u.to_le_bytes());
            }
            while !b.len().is_multiple_of(4) {
                b.push(0);
            }
            b.extend(value);
            for child in children {
                while !b.len().is_multiple_of(4) {
                    b.push(0);
                }
                b.extend(child);
            }
            let len = b.len() as u16;
            let value_len = if text { value.len() / 2 } else { value.len() } as u16;
            b[0..2].copy_from_slice(&len.to_le_bytes());
            b[2..4].copy_from_slice(&value_len.to_le_bytes());
            b[4..6].copy_from_slice(&(text as u16).to_le_bytes());
            b
        }
        let text = |s: &str| -> Vec<u8> {
            s.encode_utf16()
                .chain([0])
                .flat_map(u16::to_le_bytes)
                .collect()
        };
        let mut fixed = vec![0u8; 52];
        fixed[0..4].copy_from_slice(&0xFEEF_04BDu32.to_le_bytes());
        fixed[8..12].copy_from_slice(&((1u32 << 16) | 2).to_le_bytes());
        fixed[12..16].copy_from_slice(&((3u32 << 16) | 4).to_le_bytes());
        let strings = block(
            "040904b0",
            &[],
            true,
            &[
                block("CompanyName", &text("Example Ltd"), true, &[]),
                block("FileDescription", &text("Example Tool"), true, &[]),
            ],
        );
        let info = block(
            "VS_VERSION_INFO",
            &fixed,
            false,
            &[block("StringFileInfo", &[], true, &[strings])],
        );
        let rows = version_rows(&info);
        assert_eq!(
            rows,
            vec![
                ("Description".to_string(), "Example Tool".to_string()),
                ("Company".to_string(), "Example Ltd".to_string()),
                // No FileVersion text: the numbers.
                ("File version".to_string(), "1.2.3.4".to_string()),
            ]
        );
    }
}
