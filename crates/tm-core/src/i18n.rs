//! Program-wide translations (German + English) and language selection.
//!
//! Strings are declared once via the [`keys!`] macro which generates the
//! [`K`] key enum plus the lookup table. The active language is process-global
//! (`set_lang`), defaults to the OS language, and can be switched live from
//! the settings dialog.

use std::sync::atomic::{AtomicU8, Ordering};

use crate::locale;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    De,
    En,
}

impl Lang {
    pub fn toggle(self) -> Self {
        match self {
            Lang::De => Lang::En,
            Lang::En => Lang::De,
        }
    }
    pub fn code(self) -> &'static str {
        match self {
            Lang::De => "de",
            Lang::En => "en",
        }
    }
}

/// User-facing language preference persisted in settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum LangChoice {
    #[default]
    System,
    De,
    En,
}

impl LangChoice {
    /// Resolve against the detected OS language.
    pub fn resolve(self) -> Lang {
        match self {
            LangChoice::De => Lang::De,
            LangChoice::En => Lang::En,
            LangChoice::System => system_lang(),
        }
    }
}

/// Language implied by the OS regional settings.
pub fn system_lang() -> Lang {
    if locale::is_german() {
        Lang::De
    } else {
        Lang::En
    }
}

static ACTIVE_LANG: AtomicU8 = AtomicU8::new(0xFF); // 0xFF = unresolved

/// Install the active language (startup + live switching).
pub fn set_lang(lang: Lang) {
    ACTIVE_LANG.store(lang as u8, Ordering::Relaxed);
}

/// Active language; resolves lazily from the OS default before `set_lang`.
pub fn lang() -> Lang {
    match ACTIVE_LANG.load(Ordering::Relaxed) {
        0 => Lang::De,
        1 => Lang::En,
        _ => system_lang(),
    }
}

/// Translate `key` into the **active** language.
pub fn tr(key: K) -> &'static str {
    tr_in(lang(), key)
}

/// Translate `key` into an explicit language.
#[allow(clippy::match_same_arms)]
pub fn tr_in(lang: Lang, key: K) -> &'static str {
    let (de, en) = lookup(key);
    match lang {
        Lang::De => de,
        Lang::En => en,
    }
}

/// Translate and substitute each `{}` placeholder with the given arguments
/// in order.
pub fn trf(key: K, args: &[&str]) -> String {
    let mut s = tr(key).to_string();
    for a in args {
        s = s.replacen("{}", a, 1);
    }
    s
}

/// Declare every translatable string: `Key => ["deutsch", "english"]`.
macro_rules! keys {
    ($( $key:ident => [$de:expr, $en:expr] ),* $(,)?) => {
        /// Translation keys.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        #[allow(dead_code)]
        pub enum K { $( $key ),* }

        const fn lookup(key: K) -> (&'static str, &'static str) {
            match key {
                $( K::$key => ($de, $en) ),*
            }
        }
    };
}

keys! {
    // ------------------------------------------------ tabs & chrome
    TabProcesses => ["Prozesse", "Processes"],
    TabPerformance => ["Leistung", "Performance"],
    TabAppHistory => ["App-Verlauf", "App history"],
    TabStartup => ["Autostart von Apps", "Startup apps"],
    TabUsers => ["Benutzer", "Users"],
    TabDetails => ["Details", "Details"],
    TabServices => ["Dienste", "Services"],
    SearchHint => ["Nach Namen, Herausgeber oder PID suchen", "Type a name, publisher or PID to search"],
    Settings => ["Einstellungen", "Settings"],
    GatheringData => ["Sammle Daten…", "Gathering data…"],
    TrayOpen => ["TaskMan öffnen", "Open TaskMan"],
    TrayExit => ["TaskMan beenden", "Exit TaskMan"],

    // ------------------------------------------------ command bar
    RunNewTask => ["Neuen Task ausführen", "Run new task"],
    EndTask => ["Task beenden", "End task"],
    EndTree => ["Struktur beenden", "End tree"],
    EfficiencyMode => ["Effizienzmodus", "Efficiency mode"],
    GoToDetails => ["Gehe zu Details", "Go to details"],
    GoToServices => ["Gehe zu Dienst(en)", "Go to service(s)"],
    Expand => ["Erweitern", "Expand"],
    Collapse => ["Reduzieren", "Collapse"],
    ExpandAll => ["Alle erweitern", "Expand all"],
    CollapseAll => ["Alle reduzieren", "Collapse all"],
    ViewMode => ["Ansicht", "View"],
    FlatView => ["Flache Liste", "Flat list"],
    GroupedView => ["Nach Typ gruppiert", "Grouped by type"],
    ProcessTreeView => ["Prozessstruktur", "Process tree"],
    HierarchicalOrder => ["Streng hierarchisch (unsortiert)", "Strict hierarchy (unsorted)"],
    RefreshNow => ["Jetzt aktualisieren (F5)", "Refresh now (F5)"],
    OpenFileLocation => ["Dateispeicherort öffnen", "Open file location"],
    CreateDumpFile => ["Speicherabbild erstellen", "Create dump file"],
    Modules => ["Module", "Modules"],
    ViewModules => ["Module anzeigen", "View modules"],
    UnloadModule => ["Modul entladen", "Unload module"],
    UnloadModuleConfirm => [
        "\"{}\" wirklich aus \"{}\" entladen?",
        "Unload \"{}\" from \"{}\"?"
    ],
    UnloadModuleWarning => [
        "Das erzwungene Entladen kann den Zielprozess sofort abstürzen lassen oder beschädigen. Nur zu Diagnosezwecken verwenden.",
        "Forced unloading can immediately crash or corrupt the target process. Use only for diagnostics."
    ],
    ModuleUnloadedMsg => ["Modul entladen: {}", "Module unloaded: {}"],
    ModuleBusy => ["Eine Modulaktion läuft bereits", "A module action is already running"],
    ModuleProtected => [
        "Programmabbilder und Windows-Systemmodule werden nicht entladen",
        "Program images and Windows system modules cannot be unloaded"
    ],
    SelectModuleFirst => ["Zuerst ein Modul auswählen", "Select a module first"],
    CopyPath => ["Pfad kopieren", "Copy path"],
    Properties => ["Eigenschaften", "Properties"],
    OnlineSearch => ["Online suchen", "Search online"],
    CopyName => ["Namen kopieren", "Copy name"],
    Copied => ["Kopiert", "Copied"],
    OpenServicesApp => ["Dienste öffnen", "Open Services"],
    DisconnectUser => ["Trennen", "Disconnect"],
    SignOut => ["Abmelden", "Sign out"],
    SignOutConfirm => [
        "\"{}\" wird abgemeldet. Nicht gespeicherte Daten gehen verloren.",
        "\"{}\" will be signed out. Unsaved data will be lost."]
    ,
    EndProcessConfirm => [
        "\"{}\" (PID {}) wirklich beenden? Nicht gespeicherte Daten dieses Prozesses gehen verloren.",
        "End \"{}\" (PID {})? Unsaved data in this process will be lost."
    ],
    ManageUserAccounts => ["Benutzerkonten verwalten", "Manage user accounts"],
    EnableCmd => ["Aktivieren", "Enable"],
    DisableCmd => ["Deaktivieren", "Disable"],
    StartService => ["Starten", "Start"],
    StopService => ["Beenden", "Stop"],
    RestartService => ["Neu starten", "Restart"],

    // ------------------------------------------------ processes table
    ColName => ["Name", "Name"],
    ColStatus => ["Status", "Status"],
    ColCpu => ["CPU", "CPU"],
    ColMemory => ["Arbeitsspeicher", "Memory"],
    ColDisk => ["Datenträger", "Disk"],
    ColNetwork => ["Netzwerk", "Network"],
    ColPid => ["PID", "PID"],
    ColUsername => ["Benutzername", "User name"],
    ColPlatform => ["Plattform", "Platform"],
    ColElevated => ["Heraufgestuft", "Elevated"],
    ColUac => ["UAC-Virtualisierung", "UAC virtualization"],
    ColGpuEngine => ["GPU-Modul", "GPU engine"],
    ColPublisher => ["Herausgeber", "Publisher"],
    ColImpact => ["Startauswirkung", "Startup impact"],
    ColCpuTime => ["CPU-Zeit", "CPU time"],
    ColNotifications => ["Benachrichtigungen", "Notifications"],
    ColDescription => ["Beschreibung", "Description"],
    ColParentPid => ["Übergeordnete PID", "Parent PID"],
    ColSessionId => ["Sitzungs-ID", "Session ID"],
    ColImagePath => ["Abbildpfadname", "Image path name"],
    ColPageFaults => ["Seitenfehler/s", "Page faults/sec"],
    ColIoRead => ["E/A-Lesebytes", "I/O read bytes"],
    ColIoWrite => ["E/A-Schreibbytes", "I/O write bytes"],
    ColBaseAddress => ["Basisadresse", "Base address"],
    ColSize => ["Größe", "Size"],
    ColPath => ["Pfad", "Path"],
    ColGroup => ["Gruppe", "Group"],
    GroupApps => ["Apps", "Apps"],
    GroupBackground => ["Hintergrundprozesse", "Background processes"],
    GroupWindows => ["Windows-Prozesse", "Windows processes"],
    ColGpu => ["GPU", "GPU"],
    UacUnknown => ["Unbekannt", "Unknown"],
    UacVirtualization => ["UAC-Virtualisierung", "UAC virtualization"],
    UacVirtualizationConfirm => [
        "UAC-Virtualisierung für {} ändern? Dies kann die Funktion des Prozesses beeinflussen.",
        "Change UAC virtualization for {}? This can affect how the process works."
    ],
    UacVirtualizationChanged => [
        "UAC-Virtualisierung geändert",
        "UAC virtualization changed"
    ],
    SelectColumns => ["Spalten auswählen…", "Select columns…"],
    ColumnRequired => ["Erforderlich", "Required"],

    // ------------------------------------------------ statuses
    StRunning => ["Wird ausgeführt", "Running"],
    StSuspended => ["Angehalten", "Suspended"],
    StNotResponding => ["Nicht reagiert", "Not responding"],
    StEfficiencyMode => ["Effizienzmodus", "Efficiency mode"],
    StDisconnected => ["Getrennt", "Disconnected"],
    StIdle => ["Leerlauf", "Idle"],
    StConnected => ["Verbunden", "Connected"],
    StStartPending => ["Startet", "Starting"],
    StStopPending => ["Wird beendet", "Stopping"],
    StContinuePending => ["Wird fortgesetzt", "Resuming"],
    StPausePending => ["Wird angehalten", "Pausing"],
    StStopped => ["Beendet", "Stopped"],
    Yes => ["Ja", "Yes"],
    No => ["Nein", "No"],
    NotAllowed => ["Nicht zugelassen", "Not allowed"],
    DisabledWord => ["Deaktiviert", "Disabled"],
    EnabledWord => ["Aktiviert", "Enabled"],

    // ------------------------------------------------ details context menu
    Priority => ["Priorität", "Priority"],
    PrioRealtime => ["Echtzeit", "Realtime"],
    PrioHigh => ["Hoch", "High"],
    PrioAboveNormal => ["Über normal", "Above normal"],
    PrioNormal => ["Normal", "Normal"],
    PrioBelowNormal => ["Unter normal", "Below normal"],
    PrioLow => ["Niedrig", "Low"],
    SavePriorityForProgram => [
        "Priorität für dieses Programm speichern",
        "Save priority for this program"
    ],
    SetAffinity => ["Affinität festlegen…", "Set affinity…"],
    SaveAffinityForProgram => [
        "Affinität für dieses Programm speichern",
        "Save affinity for this program"
    ],
    SaveRuleNeedsPath => [
        "Nur für Prozesse mit bekanntem Abbildpfad verfügbar",
        "Available only when the process image path is known"
    ],
    SuspendProc => ["Anhalten", "Suspend"],
    ResumeProc => ["Fortsetzen", "Resume"],
    AffinityTitle => ["Prozessoraffinität — PID", "Processor affinity — PID"],
    AffinityWarn => ["Mindestens ein Prozessor muss ausgewählt sein.", "At least one processor must be selected."],
    Apply => ["Übernehmen", "Apply"],
    Cancel => ["Abbrechen", "Cancel"],
    Close => ["Schließen", "Close"],
    Ok => ["OK", "OK"],
    Reset => ["Zurücksetzen", "Reset"],
    Browse => ["Durchsuchen…", "Browse…"],

    // ------------------------------------------------ run dialog
    RunDialogTitle => ["Neuen Task ausführen", "Create new task"],
    RunPrompt => ["Name des Programms, Ordners oder Dokuments:", "Type the name of a program, folder or document:"],
    RunHint => ["z. B. notepad", "e.g. notepad"],
    RunElevated => ["Mit Administratorrechten ausführen", "Create this task with administrative privileges"],
    StartedToast => ["Gestartet:", "Started:"],
    LaunchFailed => ["Starten fehlgeschlagen", "Failed to launch"],

    // ------------------------------------------------ settings dialog
    DesignHeading => ["Design", "Appearance"],
    ThemeSystem => ["System", "System"],
    ThemeLight => ["Hell", "Light"],
    ThemeDark => ["Dunkel", "Dark"],
    UpdateSpeedHeading => ["Aktualisierungsgeschwindigkeit", "Update speed"],
    SpdHigh => ["Hoch (0,5 s)", "High (0.5 s)"],
    SpdNormal => ["Normal (1 s)", "Normal (1 s)"],
    SpdLow => ["Niedrig (4 s)", "Low (4 s)"],
    SpdPaused => ["Angehalten", "Paused"],
    GraphWindowLabel => ["Diagrammfenster:", "Graph window:"],
    DefaultStartPageLabel => ["Startseite:", "Default start page:"],
    ProcessViewLabel => ["Prozessansicht:", "Process view:"],
    DetailsViewLabel => ["Detailansicht:", "Details view:"],
    CpuGraphOverall => ["Gesamtauslastung", "Overall utilization"],
    CpuGraphLogical => ["Logische Prozessoren", "Logical processors"],
    ShowKernelTimes => ["Kernelzeiten anzeigen", "Show kernel times"],
    MinShort => ["Min.", "min"],
    ScaleLabel => ["Skalierung:", "Scale:"],
    TextSmoothingLabel => ["Schriftglättung:", "Font smoothing:"],
    SmoothingSharp => ["Scharf", "Sharp"],
    SmoothingStandard => ["Standard", "Standard"],
    SmoothingSmooth => ["Weich", "Smooth"],
    TextSmoothingHint => [
        "Graustufen-Kantenglättung. Subpixel-Darstellung (ClearType) unterstützt die Oberfläche nicht.",
        "Grayscale anti-aliasing. Sub-pixel rendering (ClearType) is not supported by the UI toolkit."
    ],
    NetPerProcessUnavailable => [
        "Netzwerk pro Prozess benötigt Administratorrechte (ETW-Sitzung).",
        "Per-process network needs administrator rights (ETW session)."
    ],
    RenderModeLabel => ["Grafikausgabe:", "Graphics:"],
    RenderAuto => ["Automatisch (GPU)", "Automatic (GPU)"],
    RenderCompat => ["Kompatibilität (OpenGL)", "Compatibility (OpenGL)"],
    RenderSoftware => ["Software (CPU)", "Software (CPU)"],
    RenderModeHint => [
        "Kompatibilität nutzt einen anderen Treiberpfad, falls Direct3D Probleme macht.",
        "Compatibility uses a different driver path in case Direct3D misbehaves."
    ],
    RenderSoftwareWarning => [
        "Software-Rendering rechnet jedes Bild auf der CPU und ist um ein Vielfaches langsamer (auf diesem System ~3 Bilder/s bei hoher CPU-Last).",
        "Software rendering computes every frame on the CPU and is far slower (~3 frames/s at high CPU load on this system)."
    ],
    RestartRequired => [
        "Wird beim nächsten Start übernommen.",
        "Takes effect at the next start."
    ],
    LanguageLabel => ["Sprache:", "Language:"],
    AlwaysOnTop => ["Immer im Vordergrund", "Always on top"],
    CloseToTray => [
        "Beim Schließen in den Infobereich minimieren",
        "Close to the notification area"
    ],
    StartWithWindows => [
        "Mit Windows im Infobereich starten",
        "Start with Windows in the notification area"
    ],
    SaveConfigAuto => [
        "Einstellungen automatisch in config.ini speichern",
        "Save settings automatically to config.ini"
    ],
    RememberWindow => [
        "Fenstergröße und -position merken",
        "Remember window size and position"
    ],
    ColumnsHeading => ["Spaltenbreiten zurücksetzen", "Column widths"],
    ResetColWidths => ["Standardbreiten", "Default widths"],
    ColWidthsResetToast => ["Spaltenbreiten zurückgesetzt", "Column widths reset"],
    ElevatedHeading => ["Administratorrechte", "Administrator privileges"],
    ElevatedRunning => [
        "Mit Administratorrechten ausgeführt.",
        "Running with administrator privileges."
    ],
    ElevatedNotRunning => [
        "Ohne Administratorrechte ausgeführt; Aktionen für erhöhte Prozesse sind eingeschränkt.",
        "Running without administrator privileges; actions on elevated processes are limited."
    ],
    RestartElevated => ["Als Administrator neu starten", "Restart as administrator"],
    StartElevated => [
        "Immer mit Administratorrechten starten",
        "Always start with administrator privileges"
    ],
    CoreServiceHeading => ["TaskMan-Core-Dienst", "TaskMan core service"],
    CheckingAdvancedState => ["Status wird geprüft…", "Checking status…"],
    CoreServiceNotInstalled => [
        "Nicht installiert. Geschützte Aktionen verwenden bei Bedarf weiterhin die Benutzerrechte oder UAC.",
        "Not installed. Protected actions continue to use the user token or UAC when needed."
    ],
    CoreServiceStopped => ["Installiert, aber beendet.", "Installed but stopped."],
    CoreServiceStarting => ["Wird gestartet…", "Starting…"],
    CoreServiceRunning => [
        "Aktiv (Version {}). Die Oberfläche kann ohne Administratorrechte bleiben.",
        "Active (version {}). The GUI can remain unelevated."
    ],
    CoreServiceDegraded => ["Dienstproblem: {}", "Service problem: {}"],
    CoreServiceForeignClient => [
        "Der Dienst läuft, aber dieses TaskMan-Programm ist nicht die installierte Kopie — der Dienst akzeptiert nur den geschützten Installationsort, etwa nach einem Neubau oder beim Start aus dem Build-Ordner. 'Reparieren' installiert diese Version, 'Wechseln' startet die installierte Kopie.",
        "The service is running, but this TaskMan executable is not the installed copy — the service only accepts the protected install location, e.g. after a rebuild or when started from a build folder. 'Repair' installs this build; 'Switch' starts the installed copy."
    ],
    CoreServiceSwitchRequested => [
        "Wechsle zur installierten Kopie…",
        "Switching to the installed copy…"
    ],
    CoreServiceRepairSwitchRequested => [
        "Installiere diese Version; wechsle danach zur installierten Kopie…",
        "Installing this build; switching to the installed copy afterwards…"
    ],
    InstallCoreService => ["Core-Dienst installieren…", "Install core service…"],
    RepairCoreService => ["Core-Dienst reparieren…", "Repair core service…"],
    SwitchToInstalledCoreService => [
        "Zur installierten Kopie wechseln…",
        "Switch to installed copy…"
    ],
    RemoveCoreService => ["Core-Dienst entfernen…", "Remove core service…"],
    CoreServiceInstallRequested => [
        "Dienstinstallation angefordert. TaskMan danach schließen und neu öffnen; der geschützte Start erfolgt automatisch.",
        "Service installation requested. Close and reopen TaskMan afterward; the protected copy starts automatically."
    ],
    CoreServiceRemoveRequested => [
        "Dienstentfernung angefordert",
        "Service removal requested"
    ],
    RelaunchElevatedToast => [
        "Neustart mit Administratorrechten…",
        "Restarting with administrator privileges…"
    ],

    // ------------------------------------------------ startup tab
    LastBiosTime => ["Letzte BIOS-Zeit:", "Last BIOS time:"],
    SecondsSuffix => ["Sekunden", "seconds"],
    ImpactNone => ["Keine", "None"],
    ImpactLow => ["Niedrig", "Low"],
    ImpactMedium => ["Mittel", "Medium"],
    ImpactHigh => ["Hoch", "High"],
    ImpactUnknown => ["Nicht gemessen", "Not measured"],
    StartupUnavailable => ["Autostart nicht verfügbar:", "Startup apps unavailable:"],
    PropCommand => ["Befehl:", "Command:"],
    PropLocation => ["Speicherort:", "Location:"],

    // ------------------------------------------------ services tab
    ServicesUnavailable => ["Dienste nicht verfügbar:", "Services unavailable:"],
    ServiceDoneToast => ["ausgeführt", "action completed for"],
    ActionFailed => ["Aktion fehlgeschlagen", "Action failed"],

    // ------------------------------------------------ users tab
    SessionsUnavailable => ["Sitzungen nicht verfügbar:", "Sessions unavailable:"],
    SessionDisconnected => ["Sitzung getrennt", "Session disconnected"],
    UserSignedOut => ["Benutzer abgemeldet", "User signed out"],

    // ------------------------------------------------ app history
    HistorySinceLine => ["Ressourcenauslastung seit", "Resource usage since"],
    HistoryForAccounts => ["für aktuelle Benutzer- und Systemkonten.", "for the current user and system accounts."],
    ClearHistoryLink => ["Auslastungsverlauf löschen", "Delete usage history"],
    HistoryCleared => ["Auslastungsverlauf gelöscht", "Usage history deleted"],
    HistoryLocalNote => [
        "Lokaler Verlauf, der nur während der Ausführung von Taskman erfasst wird; der Windows-SRUM-Verlauf wird nicht importiert.",
        "Local history collected only while Taskman is running; Windows SRUM history is not imported."
    ],

    // ------------------------------------------------ performance tab
    Utilization60sPct => ["Auslastung in 60 Sekunden (%)", "Utilization for 60 seconds (%)"],
    MemUsage60s => ["Speicherauslastung in 60 Sekunden", "Memory usage for 60 seconds"],
    CommittedMem => ["Zugesicherter Speicher", "Committed memory"],
    TransferRate60s => ["Übertragungsrate in 60 Sekunden (KB/s)", "Transfer rate for 60 seconds (KB/s)"],
    ActiveTime60s => ["Aktive Zeit in 60 Sekunden", "Active time for 60 seconds"],
    Receive60s => ["Empfangen in 60 Sekunden (KBit/s)", "Receive for 60 seconds (Mbps)"],
    Send60s => ["Senden in 60 Sekunden (KBit/s)", "Send for 60 seconds (Mbps)"],
    GpuMem60s => ["GPU-Speicher in 60 Sekunden", "GPU memory for 60 seconds"],
    StatUtilization => ["Auslastung", "Utilization"],
    StatSpeed => ["Geschwindigkeit", "Speed"],
    StatProcesses => ["Prozesse", "Processes"],
    StatThreads => ["Threads", "Threads"],
    StatHandles => ["Handles", "Handles"],
    StatUptime => ["Betriebszeit", "Up time"],
    KvBaseSpeed => ["Basisgeschwindigkeit:", "Base speed:"],
    KvSockets => ["Sockets:", "Sockets:"],
    KvCores => ["Kerne:", "Cores:"],
    KvLogical => ["Logische Prozessoren:", "Logical processors:"],
    KvVirtualization => ["Virtualisierung:", "Virtualization:"],
    VirtEnabled => ["Aktiviert", "Enabled"],
    VirtDisabled => ["Deaktiviert", "Disabled"],
    MemTitle => ["Arbeitsspeicher", "Memory"],
    StatInUse => ["In Verwendung", "In use"],
    StatCommitted => ["Zugesichert", "Committed"],
    StatCached => ["Zwischengespeichert", "Cached"],
    StatPagedPool => ["Ausgelagerter Pool", "Paged pool"],
    StatNonPagedPool => ["Nicht ausgelagerter Pool", "Non-paged pool"],
    KvTotal => ["Gesamt:", "Total:"],
    KvAvailable => ["Verfügbar:", "Available:"],
    KvCommitLimit => ["Commit-Limit:", "Commit limit:"],
    KvPagefile => ["Auslagerungsdatei:", "Page file:"],
    KvRamSpeed => ["Geschwindigkeit:", "Speed:"],
    KvSlotsUsed => ["Belegte Steckplätze:", "Slots used:"],
    KvFormFactor => ["Bauform:", "Form factor:"],
    KvHwReserved => ["Für Hardware reserviert:", "Hardware reserved:"],
    KvAdapter => ["Adapter:", "Adapter:"],
    DiskTitle => ["Datenträger", "Disk"],
    StatActiveTime => ["Aktive Zeit", "Active time"],
    StatRead => ["Lesen", "Read"],
    StatWrite => ["Schreiben", "Write"],
    KvAvgResponse => ["Durchschnittl. Reaktionszeit:", "Average response time:"],
    KvCapacity => ["Kapazität:", "Capacity:"],
    KvUsedSpace => ["Belegt:", "Used space:"],
    KvFreeSpace => ["Frei:", "Free space:"],
    StatReceive => ["Empfangen", "Receive"],
    StatSend => ["Senden", "Send"],
    KvTotalReceived => ["Insgesamt empfangen:", "Total received:"],
    KvTotalSent => ["Insgesamt gesendet:", "Total sent:"],
    KvLinkSpeed => ["Verbindungsgeschwindigkeit:", "Connection speed:"],
    KvIpv4 => ["IPv4-Adresse:", "IPv4 address:"],
    KvIpv6 => ["IPv6-Adresse:", "IPv6 address:"],
    KvSignal => ["Signalstärke:", "Signal strength:"],
    ThroughputWindow => ["Durchsatz, {}", "Throughput, {}"],
    CardSentRecv => ["Ges.: {}  Empf.: {}", "S: {}  R: {}"],
    GpuTitle => ["GPU", "GPU"],
    GpuMemStat => ["GPU-Speicher", "GPU memory"],
    KvDedicatedMem => ["Dedizierter Speicher:", "Dedicated memory:"],
    KvTemperature => ["Temperatur:", "Temperature:"],
    KvDriverVersion => ["Treiberversion:", "Driver version:"],
    KvEnginePrefix => ["Engine", "Engine"],

    // ------------------------------------------------ toasts
    ProcessEndedToast => ["Prozess {} beendet", "Process {} ended"],
    NameEndedToast => ["{} beendet", "{} ended"],
    TreeOfEndedToast => ["Struktur von {} beendet", "Tree of {} ended"],
    StartedMsg => ["Gestartet: {}", "Started: {}"],
    ErrMsg => ["Fehler: {}", "Error: {}"],
    DumpWrittenMsg => ["Speicherabbild gespeichert: {}", "Dump written: {}"],
    DumpAlreadyRunning => ["Ein Speicherabbild wird bereits erstellt", "A dump is already being created"],
    NoServiceForPid => ["Kein Dienst mit PID {} gefunden", "No service with PID {} found"],
    NoFileForProcess => ["Kein Dateipfad verfügbar", "No file path available"],
    ProcessExited => ["(Prozess wurde beendet)", "(process has exited)"],
    // Pseudo-rows for CPU time no live process can be charged with
    // (interrupt/DPC servicing; programs that terminated during the
    // sampling window — compiler churn, scripts, installers).
    SystemInterrupts => ["Systeminterrupts", "System interrupts"],
    TerminatedProcesses => ["Beendete Prozesse ({})", "Terminated processes ({})"],
    TerminatedProcessesPlain => ["Beendete Prozesse", "Terminated processes"],
    TerminatedTooltip => ["CPU von Prozessen, die sich seit der letzten Aktualisierung beendet haben: {}", "CPU of processes that exited since the last refresh: {}"],
    PropPath => ["Pfad:", "Path:"],
    PrioritySetMsg => ["Priorität: {}", "Priority: {}"],
    ProcessEnded => ["beendet", "ended"],
    TreeEndedFor => ["Struktur von", "Tree of"],
    TreeEnded => ["Struktur beendet", "Tree ended"],
    DumpWritten => ["Speicherabbild gespeichert:", "Dump written:"],
    DumpFailed => ["Speicherabbild fehlgeschlagen", "Failed to create dump"],
    ErrorPrefix => ["Fehler:", "Error:"],
    PrioritySet => ["Priorität:", "Priority:"],
    AffinitySet => ["Affinität gesetzt", "Affinity updated"],
    SavedProcessRuleFailed => [
        "Gespeicherte Prozessregel für PID {} konnte nicht angewendet werden: {}",
        "Could not apply saved process rule for PID {}: {}"
    ],
    ActionQueueFull => [
        "Zu viele Systemaktionen warten; bitte erneut versuchen",
        "Too many system actions are queued; please try again"
    ],
    EfficiencyChanged => ["Effizienzmodus geändert", "Efficiency mode changed"],

    // ------------------------------------------------ misc words
    Bit32 => ["32 Bit", "32-bit"],
    Bit64 => ["64 Bit", "64-bit"],

    // ------------------------------------------------ window title
    WindowTitle => ["Task-Manager", "Task Manager"],
}

/// Unit words that follow the UI language (used by the formatters).
pub fn unit_mbit_per_s() -> &'static str {
    match lang() {
        Lang::De => "MBit/s",
        Lang::En => "Mbps",
    }
}

pub fn unit_kbit() -> &'static str {
    match lang() {
        Lang::De => "KBit",
        Lang::En => "kbps",
    }
}
