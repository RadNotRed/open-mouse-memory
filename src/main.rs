use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use open_mouse_memory::access;
use open_mouse_memory::device::{
    LogicalDevice, discover, dpi_capabilities, rate_capabilities, read_battery, read_dpi, read_firmware,
    read_identity, read_onboard_backup, read_onboard_info, read_onboard_profiles, read_onboard_sector,
    read_onboard_status, read_rate, select_device, set_dpi, set_onboard_active_profile,
    set_onboard_current_dpi_index, set_onboard_mode, set_rate, verify_onboard_sector_write,
    write_onboard_profile,
};
use open_mouse_memory::error::{AppError, Result};
use open_mouse_memory::hid::descriptor;
use open_mouse_memory::hid::transport::HidDeviceIo;
use open_mouse_memory::hid::{HidEndpoint, refresh_api};
use open_mouse_memory::hidpp::features::{ONBOARD_PROFILES, feature_name};
use open_mouse_memory::hidpp::protocol::spaced_hex;
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(
    name = "open-mouse-memory",
    version,
    about = "Open Mouse Memory CLI for Logitech HID++ mice"
)]
struct Cli {
    /// choose a device by id name serial or hidraw path
    #[arg(short = 'd', long, global = true)]
    device: Option<String>,

    /// output json
    #[arg(long, global = true)]
    json: bool,

    /// print raw hid++ messages to stderr
    #[arg(long, global = true)]
    trace: bool,

    /// disable access approval prompts
    #[arg(long, global = true)]
    no_access_prompt: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// list logitech hid devices
    Devices,
    /// read device details
    Device {
        #[command(subcommand)]
        command: DeviceCommand,
    },
    /// print battery level
    Battery(BatteryArgs),
    /// read or set sensor dpi
    Dpi {
        #[command(subcommand)]
        command: DpiCommand,
    },
    /// read or set report rate
    Rate {
        #[command(subcommand)]
        command: RateCommand,
    },
    /// view or switch onboard mode
    Onboard {
        #[command(subcommand)]
        command: OnboardCommand,
    },
    /// hid++ debug tools
    Debug {
        #[command(subcommand)]
        command: DebugCommand,
    },
    #[command(name = "__install-access-rule", hide = true)]
    InternalInstallAccessRule,
}

#[derive(Debug, Args)]
struct BatteryArgs {
    /// show battery details
    #[arg(long)]
    details: bool,
}

#[derive(Debug, Subcommand)]
enum DeviceCommand {
    Info,
    Features,
    Firmware,
    Battery,
}

#[derive(Debug, Subcommand)]
enum DpiCommand {
    Get,
    Capabilities,
    Set(DpiSetArgs),
}

#[derive(Debug, Args)]
struct DpiSetArgs {
    /// dpi for both axes or x with --y
    value: u16,
    /// optional y axis dpi
    #[arg(long)]
    y: Option<u16>,
}

#[derive(Debug, Subcommand)]
enum RateCommand {
    Get,
    Capabilities,
    Set { hz: u32 },
}

#[derive(Debug, Subcommand)]
enum OnboardCommand {
    Status,
    Info {
        /// show raw 16-byte capability data
        #[arg(long)]
        raw: bool,
    },
    Enable,
    Disable,
    Profiles {
        #[command(subcommand)]
        command: OnboardProfilesCommand,
    },
}

#[derive(Debug, Subcommand)]
enum OnboardProfilesCommand {
    List,
    Dump { sector: u16 },
    Backup { path: PathBuf },
    Activate { slot: u8 },
    Stage { stage: u8 },
    SetRate { slot: u8, hz: u32 },
    VerifyWrite { sector: u16 },
}

#[derive(Debug, Subcommand)]
enum DebugCommand {
    /// decode a hid report descriptor
    Descriptor {
        /// hidraw path or selected device
        path: Option<String>,
    },
    /// show hid++ features
    HidppFeatures,
    /// send one feature call
    RawCommand(RawCommandArgs),
}

#[derive(Debug, Args)]
struct RawCommandArgs {
    /// hid++ feature id in hex such as 8100
    #[arg(long, value_parser = parse_hex_u16)]
    feature: u16,
    /// function number or wire value in hex
    #[arg(long, value_parser = parse_function)]
    function: u8,
    /// data bytes in hex with spaces or colons
    #[arg(long, default_value = "")]
    data: String,
    /// mark the function as read only
    #[arg(long)]
    read_only: bool,
    /// allow a possible write
    #[arg(long)]
    dangerous_write: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if matches!(cli.command, Command::InternalInstallAccessRule) {
        return finish(&cli, access::install_rule_as_root());
    }

    let mut result = run(&cli);
    if access::prompt_allowed(cli.no_access_prompt, cli.json) {
        if let Err(AppError::PermissionDenied { path }) = &result {
            result = match access::prompt_and_request(path) {
                Ok(true) => run(&cli),
                Ok(false) => result,
                Err(error) => Err(error),
            };
        }
    }
    finish(&cli, result)
}

fn finish(cli: &Cli, result: Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if cli.json {
                let value = serde_json::json!({
                    "error": error.to_string(),
                    "exit_code": error.exit_code(),
                });
                eprintln!("{}", serde_json::to_string_pretty(&value).unwrap());
            } else {
                eprintln!("ERROR: {error}");
            }
            ExitCode::from(error.exit_code())
        }
    }
}

fn run(cli: &Cli) -> Result<()> {
    let api = refresh_api()?;
    let discovery = discover(&api, cli.trace);
    if let Command::Devices = cli.command {
        if discovery.endpoints.is_empty() {
            return Err(AppError::NoDevice);
        }
        return print_devices(
            cli.json,
            &discovery.endpoints,
            &discovery.devices,
            &discovery.probe_errors,
        );
    }

    if let Command::Debug {
        command: DebugCommand::Descriptor { path },
    } = &cli.command
    {
        return print_descriptor(
            cli.json,
            &api,
            &discovery.endpoints,
            &discovery.devices,
            cli.device.as_deref(),
            path.as_deref(),
        );
    }

    let device = match select_device(&discovery.devices, cli.device.as_deref()) {
        Err(AppError::NoDevice) if !discovery.permission_denied_paths.is_empty() => {
            return Err(AppError::PermissionDenied {
                path: discovery.permission_denied_paths[0].clone(),
            });
        }
        result => result?,
    };
    let mut transport = device.open(&api, cli.trace)?;
    match &cli.command {
        Command::Device { command } => match command {
            DeviceCommand::Info => output(cli.json, &read_identity(&mut transport, device)?, print_identity),
            DeviceCommand::Features => output(cli.json, &device.features.features, |features| {
                println!("Index  Feature  Version  Type  Name");
                for feature in features {
                    println!(
                        "0x{:02X}  0x{:04X}   {:>3}      0x{:02X}  {}",
                        feature.index, feature.id, feature.version, feature.feature_type, feature.name
                    );
                }
            }),
            DeviceCommand::Firmware => output(
                cli.json,
                &read_firmware(&mut transport, &device.features)?,
                |items| {
                    for item in items {
                        println!(
                            "{}: {}{}",
                            item.kind,
                            item.name
                                .as_deref()
                                .map(|name| format!("{name} "))
                                .unwrap_or_default(),
                            item.version.as_deref().unwrap_or("unknown")
                        );
                    }
                },
            ),
            DeviceCommand::Battery => output(
                cli.json,
                &read_battery(&mut transport, &device.features)?,
                print_battery_details,
            ),
        },
        Command::Battery(args) => output(
            cli.json,
            &read_battery(&mut transport, &device.features)?,
            |battery| {
                if args.details {
                    print_battery_details(battery);
                } else {
                    println!("{}", battery_level_text(battery));
                }
            },
        ),
        Command::Dpi { command } => match command {
            DpiCommand::Get => output(cli.json, &read_dpi(&mut transport, &device.features)?, |dpi| {
                println!("X: {} DPI", dpi.x);
                println!("Y: {} DPI", dpi.y);
                if let Some(lod) = dpi.lift_off_distance {
                    println!("Lift-off distance code: {lod}");
                }
            }),
            DpiCommand::Capabilities => output(
                cli.json,
                &dpi_capabilities(&mut transport, &device.features)?,
                |capabilities| {
                    println!("Minimum: {} DPI", capabilities.minimum);
                    println!("Maximum: {} DPI", capabilities.maximum);
                    println!(
                        "Step: {}",
                        capabilities
                            .step
                            .map(|step| format!("{step} DPI"))
                            .unwrap_or_else(|| "variable".to_owned())
                    );
                    println!("Separate X/Y: {}", yes_no(capabilities.separate_xy));
                    println!("Lift-off distance: {}", yes_no(capabilities.lift_off_distance));
                    println!("Values: {}", summarize_numbers(&capabilities.x_values));
                },
            ),
            DpiCommand::Set(args) => output(
                cli.json,
                &set_dpi(&mut transport, &device.features, args.value, args.y)?,
                |dpi| {
                    println!("DPI set and verified: {} x {}", dpi.x, dpi.y);
                },
            ),
        },
        Command::Rate { command } => match command {
            RateCommand::Get => output(cli.json, &read_rate(&mut transport, &device.features)?, |rate| {
                println!("{} Hz ({} us)", rate.hz, rate.interval_microseconds);
            }),
            RateCommand::Capabilities => output(
                cli.json,
                &rate_capabilities(&mut transport, &device.features)?,
                |capabilities| {
                    for rate in &capabilities.rates_hz {
                        println!("{rate} Hz");
                    }
                },
            ),
            RateCommand::Set { hz } => output(
                cli.json,
                &set_rate(&mut transport, &device.features, *hz)?,
                |rate| {
                    println!("Report rate set and verified: {} Hz", rate.hz);
                },
            ),
        },
        Command::Onboard { command } => match command {
            OnboardCommand::Status => output(
                cli.json,
                &read_onboard_status(&mut transport, &device.features)?,
                |status| print_onboard_status(&status.mode, status.active_sector),
            ),
            OnboardCommand::Info { raw } => {
                let info = read_onboard_info(&mut transport, &device.features)?;
                if cli.json {
                    print_json(&info)
                } else {
                    println!("Memory model:      0x{:02X}", info.memory_model);
                    println!("Profile format:    0x{:02X}", info.profile_format);
                    println!("Macro format:      0x{:02X}", info.macro_format);
                    println!("Profile count:     {}", info.profile_count);
                    println!("ROM profiles:      {}", info.rom_profile_count);
                    println!("Button count:      {}", info.button_count);
                    println!("Sector count:      {}", info.sector_count);
                    println!("Sector size:       {}", info.sector_size);
                    println!("Mechanical layout: 0x{:02X}", info.mechanical_layout);
                    println!("Various info:      0x{:02X}", info.various_info);
                    println!(
                        "Reserved:          {}",
                        spaced_from_compact_hex(&info.reserved_hex)
                    );
                    if *raw {
                        println!("Raw:               {}", spaced_from_compact_hex(&info.raw_hex));
                    }
                    Ok(())
                }
            }
            OnboardCommand::Enable => output(
                cli.json,
                &set_onboard_mode(&mut transport, &device.features, true)?,
                |status| print_onboard_status(&status.mode, status.active_sector),
            ),
            OnboardCommand::Disable => output(
                cli.json,
                &set_onboard_mode(&mut transport, &device.features, false)?,
                |status| print_onboard_status(&status.mode, status.active_sector),
            ),
            OnboardCommand::Profiles { command } => match command {
                OnboardProfilesCommand::List => list_onboard_profiles(cli.json, &mut transport, device),
                OnboardProfilesCommand::Dump { sector } => {
                    let info = read_onboard_info(&mut transport, &device.features)?;
                    if *sector >= info.sector_count as u16 {
                        return Err(AppError::Validation(format!(
                            "sector {sector:#06x} is outside the device range 0x0000-{:#06x}",
                            info.sector_count.saturating_sub(1)
                        )));
                    }
                    let profile =
                        read_onboard_sector(&mut transport, &device.features, *sector, info.sector_size)?;
                    output(cli.json, &profile, |profile| {
                        println!("Sector:       0x{:04X}", profile.sector);
                        println!("Size:         {} bytes", profile.size);
                        println!("Stored CRC:   0x{:04X}", profile.stored_crc);
                        println!("Computed CRC: 0x{:04X}", profile.computed_crc);
                        println!("CRC valid:    {}", profile.crc_valid);
                        println!("Data:         {}", spaced_from_compact_hex(&profile.data_hex));
                    })
                }
                OnboardProfilesCommand::Backup { path } => {
                    let backup = read_onboard_backup(&mut transport, device)?;
                    write_json_backup(path, &backup)?;
                    if cli.json {
                        print_json(&serde_json::json!({
                            "path": path,
                            "sectors": backup.sectors.len(),
                            "profiles": backup.profiles.len(),
                        }))
                    } else {
                        println!("Backed up {} sectors to {}", backup.sectors.len(), path.display());
                        Ok(())
                    }
                }
                OnboardProfilesCommand::Activate { slot } => {
                    set_onboard_mode(&mut transport, &device.features, true)?;
                    let status = set_onboard_active_profile(&mut transport, &device.features, *slot)?;
                    output(cli.json, &status, |status| {
                        print_onboard_status(&status.mode, status.active_sector)
                    })
                }
                OnboardProfilesCommand::Stage { stage } => {
                    if !(1..=5).contains(stage) {
                        return Err(AppError::Validation(
                            "onboard stage must be between 1 and 5".to_owned(),
                        ));
                    }
                    let index = set_onboard_current_dpi_index(&mut transport, &device.features, stage - 1)?;
                    if cli.json {
                        print_json(&serde_json::json!({
                            "stage": index + 1,
                            "index": index,
                        }))
                    } else {
                        println!("Current onboard DPI stage: {}", index + 1);
                        Ok(())
                    }
                }
                OnboardProfilesCommand::SetRate { slot, hz } => {
                    let profiles = read_onboard_profiles(&mut transport, &device.features)?;
                    let mut profile = profiles
                        .into_iter()
                        .find(|profile| profile.slot == *slot)
                        .ok_or_else(|| {
                            AppError::Validation(format!("onboard profile slot {slot} does not exist"))
                        })?
                        .profile;
                    profile.report_rate = *hz;
                    let active_dpi = profile.active_dpi as u8;
                    let written = write_onboard_profile(&mut transport, &device.features, *slot, &profile)?;
                    set_onboard_mode(&mut transport, &device.features, false)?;
                    set_onboard_mode(&mut transport, &device.features, true)?;
                    set_onboard_active_profile(&mut transport, &device.features, *slot)?;
                    set_onboard_current_dpi_index(&mut transport, &device.features, active_dpi)?;
                    output(cli.json, &written, |written| {
                        println!(
                            "Saved {} Hz to onboard profile slot {} in sector 0x{:04X}",
                            hz, written.slot, written.sector
                        );
                    })
                }
                OnboardProfilesCommand::VerifyWrite { sector } => {
                    let verified = verify_onboard_sector_write(&mut transport, &device.features, *sector)?;
                    output(cli.json, &verified, |verified| {
                        println!(
                            "Sector 0x{:04X} was rewritten unchanged and verified with CRC 0x{:04X}",
                            verified.sector, verified.stored_crc
                        );
                    })
                }
            },
        },
        Command::Debug { command } => match command {
            DebugCommand::HidppFeatures => output(cli.json, &device.features, |table| {
                for feature in &table.features {
                    println!(
                        "index=0x{:02X} id=0x{:04X} version={} type=0x{:02X} {}",
                        feature.index, feature.id, feature.version, feature.feature_type, feature.name
                    );
                }
            }),
            DebugCommand::RawCommand(args) => raw_command(cli.json, &mut transport, device, args),
            DebugCommand::Descriptor { .. } => unreachable!(),
        },
        Command::Devices => unreachable!(),
        Command::InternalInstallAccessRule => unreachable!(),
    }
}

#[derive(Serialize)]
struct OnboardProfileSummary {
    slot: u8,
    sector: u16,
    enabled: bool,
    active: bool,
    crc_valid: bool,
    report_rate: u32,
    dpi_points: Vec<u16>,
    active_dpi: usize,
    shift_dpi: Option<usize>,
    buttons: Vec<String>,
}

#[derive(Serialize)]
struct OnboardProfileList {
    profile_format: u8,
    active_sector: Option<u16>,
    profiles: Vec<OnboardProfileSummary>,
}

fn list_onboard_profiles(
    json: bool,
    transport: &mut open_mouse_memory::hidpp::HidppTransport<HidDeviceIo>,
    device: &LogicalDevice,
) -> Result<()> {
    let info = read_onboard_info(transport, &device.features)?;
    let status = read_onboard_status(transport, &device.features)?;
    let profiles = read_onboard_profiles(transport, &device.features)?
        .into_iter()
        .map(|onboard| OnboardProfileSummary {
            slot: onboard.slot,
            sector: onboard.sector,
            enabled: onboard.enabled,
            active: onboard.active,
            crc_valid: onboard.crc_valid,
            report_rate: onboard.profile.report_rate,
            dpi_points: onboard.profile.dpi_points,
            active_dpi: onboard.profile.active_dpi,
            shift_dpi: onboard.profile.shift_dpi,
            buttons: onboard
                .profile
                .bindings
                .into_iter()
                .map(|binding| format!("{}: {}", binding.button.label(), binding.action.label()))
                .collect(),
        })
        .collect();
    let list = OnboardProfileList {
        profile_format: info.profile_format,
        active_sector: status.active_sector,
        profiles,
    };
    output(json, &list, |list| {
        println!("Profile format: 0x{:02X}", list.profile_format);
        for profile in &list.profiles {
            let state = if profile.active {
                "active"
            } else if profile.enabled {
                "enabled"
            } else {
                "disabled"
            };
            println!(
                "Slot {}  sector 0x{:04X}  {:8}  {} Hz  DPI {:?}  CRC {}",
                profile.slot,
                profile.sector,
                state,
                profile.report_rate,
                profile.dpi_points,
                if profile.crc_valid { "valid" } else { "INVALID" }
            );
            for button in &profile.buttons {
                println!("  {button}");
            }
        }
    })
}

fn write_json_backup(path: &Path, value: &impl Serialize) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|error| {
                AppError::Other(format!(
                    "cannot create backup directory {}: {error}",
                    parent.display()
                ))
            })?;
        }
    }
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| AppError::Other(format!("cannot encode onboard backup: {error}")))?;
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(format!(".{}.tmp", std::process::id()));
    let temporary = PathBuf::from(temporary);
    fs::write(&temporary, bytes)
        .map_err(|error| AppError::Other(format!("cannot write {}: {error}", temporary.display())))?;
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(AppError::Other(format!(
            "cannot save {}: {error}",
            path.display()
        )));
    }
    Ok(())
}

#[derive(Serialize)]
struct DevicesOutput<'a> {
    devices: &'a [LogicalDevice],
    hid_interfaces: &'a [HidEndpoint],
    probe_errors: &'a [String],
}

fn print_devices(
    json: bool,
    endpoints: &[HidEndpoint],
    devices: &[LogicalDevice],
    probe_errors: &[String],
) -> Result<()> {
    if json {
        return print_json(&DevicesOutput {
            devices,
            hid_interfaces: endpoints,
            probe_errors,
        });
    }
    if devices.is_empty() {
        println!("No responsive HID++ 2.x logical devices found.");
    } else {
        println!("Logical devices");
        for (index, device) in devices.iter().enumerate() {
            println!(
                "{}  {}  HID++ {}  receiver {:04X}:{:04X}  index 0x{:02X}  {}",
                index + 1,
                device.name,
                device.protocol,
                device.endpoint.vendor_id,
                device.endpoint.product_id,
                device.device_index,
                device.endpoint.path
            );
        }
        println!();
    }
    println!("Logitech HID interfaces");
    println!("VID:PID    Interface  Usage Page  Usage   Path             Product");
    for endpoint in endpoints {
        println!(
            "{:04X}:{:04X}  {:>9}  0x{:04X}      0x{:04X}  {:<16} {}",
            endpoint.vendor_id,
            endpoint.product_id,
            endpoint.interface_number,
            endpoint.usage_page,
            endpoint.usage,
            endpoint.path,
            endpoint.product.as_deref().unwrap_or("unknown")
        );
    }
    if devices.is_empty() && !probe_errors.is_empty() {
        eprintln!("Probe notes:");
        for error in probe_errors {
            eprintln!("  {error}");
        }
    }
    Ok(())
}

fn print_descriptor(
    json: bool,
    api: &hidapi::HidApi,
    endpoints: &[HidEndpoint],
    devices: &[LogicalDevice],
    selector: Option<&str>,
    path: Option<&str>,
) -> Result<()> {
    let endpoint = if let Some(path) = path {
        endpoints
            .iter()
            .find(|endpoint| endpoint.path == path)
            .ok_or_else(|| AppError::DeviceNotFound(path.to_owned()))?
    } else if !devices.is_empty() {
        &select_device(devices, selector)?.endpoint
    } else {
        endpoints.first().ok_or(AppError::NoDevice)?
    };
    let device = endpoint.open(api)?;
    let io = HidDeviceIo::new(device, endpoint.path.clone());
    let parsed = descriptor::inspect(&io.report_descriptor()?);
    if json {
        print_json(&parsed)
    } else {
        println!("Path: {}", endpoint.path);
        println!("Descriptor bytes: {}", parsed.byte_length);
        println!(
            "Usage pages: {}",
            parsed
                .usage_pages
                .iter()
                .map(|page| format!("0x{page:04X}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        println!(
            "Report IDs: {}",
            parsed
                .report_ids
                .iter()
                .map(|id| format!("0x{id:02X}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        println!("Input bits: {:?}", parsed.input_bits);
        println!("Output bits: {:?}", parsed.output_bits);
        println!("Feature bits: {:?}", parsed.feature_bits);
        println!("Raw: {}", spaced_from_compact_hex(&parsed.raw_hex));
        Ok(())
    }
}

fn raw_command(
    json: bool,
    transport: &mut open_mouse_memory::hidpp::HidppTransport<HidDeviceIo>,
    device: &LogicalDevice,
    args: &RawCommandArgs,
) -> Result<()> {
    if args.read_only == args.dangerous_write {
        return Err(AppError::Unsafe(
            "choose exactly one of --read-only or --dangerous-write after classifying the command".to_owned(),
        ));
    }
    if args.feature == ONBOARD_PROFILES && matches!(args.function, 0x60 | 0x70 | 0x80) {
        return Err(AppError::Unsafe(
            "ONBOARD_PROFILES memory write functions are disabled in v0.1, even with --dangerous-write"
                .to_owned(),
        ));
    }
    let data = parse_hex_bytes(&args.data).map_err(AppError::Validation)?;
    let feature = device.features.require(args.feature)?;
    let response = if args.read_only {
        transport.transact_read(feature.index, args.function, &data)?
    } else {
        transport.transact(feature.index, args.function, &data)?
    };
    if json {
        print_json(&serde_json::json!({
            "feature_id": format!("0x{:04X}", args.feature),
            "feature_name": feature_name(args.feature),
            "feature_index": feature.index,
            "function": format!("0x{:02X}", args.function),
            "request_hex": hex::encode_upper(&data),
            "response_hex": hex::encode_upper(&response),
        }))
    } else {
        println!(
            "Feature: 0x{:04X} {} (index 0x{:02X})",
            args.feature,
            feature_name(args.feature),
            feature.index
        );
        println!("Function: 0x{:02X}", args.function);
        println!("Response: {}", spaced_hex(&response));
        Ok(())
    }
}

fn print_identity(identity: &open_mouse_memory::device::DeviceIdentity) {
    println!("Name: {}", identity.name);
    println!("HID++: {}", identity.hidpp_version);
    println!("Receiver: {}:{}", identity.receiver.vid, identity.receiver.pid);
    if let Some(serial) = &identity.receiver.serial {
        println!("Receiver serial: {serial}");
    }
    println!("Path: {}", identity.receiver.path);
    println!("Device index: 0x{:02X}", identity.device_index);
    if let Some(unit_id) = &identity.unit_id {
        println!("Unit ID: {unit_id}");
    }
    if let Some(profile_format) = identity.profile_format {
        println!("Profile format: 0x{profile_format:02X}");
    }
}

fn print_onboard_status(mode: &str, active_sector: Option<u16>) {
    println!("Mode: {mode}");
    if let Some(sector) = active_sector {
        println!("Active profile sector: 0x{sector:04X}");
    }
}

fn battery_level_text(battery: &open_mouse_memory::device::BatteryInfo) -> String {
    battery
        .percentage
        .map(|level| format!("{level}%"))
        .or_else(|| battery.approximate_level.clone())
        .unwrap_or_else(|| "unknown".to_owned())
}

fn print_battery_details(battery: &open_mouse_memory::device::BatteryInfo) {
    println!("Level: {}", battery_level_text(battery));
    println!("Status: {}", battery.status);
    if let Some(voltage) = battery.voltage_mv {
        println!("Voltage: {voltage} mV");
    }
    println!("Source: {}", battery.source);
    println!("Raw: {}", spaced_from_compact_hex(&battery.raw_hex));
}

fn output<T: Serialize>(json: bool, value: &T, text: impl FnOnce(&T)) -> Result<()> {
    if json {
        print_json(value)
    } else {
        text(value);
        Ok(())
    }
}

fn print_json(value: &impl Serialize) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(value)
            .map_err(|error| AppError::Other(format!("JSON encoding failed: {error}")))?
    );
    Ok(())
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn summarize_numbers(values: &[u16]) -> String {
    let mut parts = Vec::new();
    let mut index = 0;
    while index < values.len() {
        if index + 3 < values.len() {
            let step = values[index + 1] - values[index];
            let mut end = index + 1;
            while end + 1 < values.len() && values[end + 1] - values[end] == step {
                end += 1;
            }
            if end - index >= 3 {
                parts.push(format!("{}-{} (step {step})", values[index], values[end]));
                index = end + 1;
                continue;
            }
        }
        parts.push(values[index].to_string());
        index += 1;
    }
    parts.join(", ")
}

fn spaced_from_compact_hex(value: &str) -> String {
    value
        .as_bytes()
        .chunks(2)
        .map(|chunk| String::from_utf8_lossy(chunk))
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_hex_u16(value: &str) -> std::result::Result<u16, String> {
    u16::from_str_radix(value.trim_start_matches("0x"), 16).map_err(|error| error.to_string())
}

fn parse_function(value: &str) -> std::result::Result<u8, String> {
    let parsed = u8::from_str_radix(value.trim_start_matches("0x"), 16).map_err(|error| error.to_string())?;
    if parsed <= 0x0f {
        Ok(parsed << 4)
    } else if parsed & 0x0f == 0 {
        Ok(parsed)
    } else {
        Err("function must be an ordinal from 0-F or a wire value whose low nibble is zero".to_owned())
    }
}

fn parse_hex_bytes(value: &str) -> std::result::Result<Vec<u8>, String> {
    let compact: String = value
        .chars()
        .filter(|character| !character.is_ascii_whitespace() && *character != ':')
        .collect();
    if compact.is_empty() {
        return Ok(Vec::new());
    }
    hex::decode(compact.trim_start_matches("0x")).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_function_ordinal_or_wire_value() {
        assert_eq!(parse_function("05").unwrap(), 0x50);
        assert_eq!(parse_function("0x50").unwrap(), 0x50);
        assert!(parse_function("51").is_err());
    }

    #[test]
    fn parses_friendly_hex_bytes() {
        assert_eq!(parse_hex_bytes("00:01 0A").unwrap(), [0, 1, 10]);
    }

    #[test]
    fn summarizes_long_numeric_ranges() {
        assert_eq!(
            summarize_numbers(&[100, 150, 200, 250, 300, 400]),
            "100-300 (step 50), 400"
        );
    }

    #[test]
    fn formats_numeric_battery_level_for_shells() {
        let battery = open_mouse_memory::device::BatteryInfo {
            source: "UNIFIED_BATTERY".to_owned(),
            percentage: Some(49),
            approximate_level: Some("good".to_owned()),
            next_level: None,
            status: "discharging".to_owned(),
            voltage_mv: None,
            raw_hex: "31040000".to_owned(),
        };
        assert_eq!(battery_level_text(&battery), "49%");
    }
}
