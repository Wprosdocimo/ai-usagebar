import QtQuick
import QtQuick.Layouts
import QtQuick.Controls as QQC2
import org.kde.plasma.plasma5support as Plasma5Support
import org.kde.kirigami as Kirigami
import org.kde.kcmutils as KCM
import "../code/plasmoid-logic.mjs" as Logic

// The cfg_<key> properties ARE the mechanism: Plasma copies each config value
// onto a property of that exact name here, and reads them back on Apply. A typo
// in the suffix fails silently — the setting simply never persists.
KCM.SimpleKCM {
    id: page

    property alias cfg_interval: intervalSpin.value
    property alias cfg_binaryPath: binaryField.text
    property alias cfg_terminalCommand: terminalField.text
    property alias cfg_useThemeColors: themeColorsCheck.checked
    property alias cfg_showBars: showBarsCheck.checked
    property alias cfg_barWidth: barWidthSpin.value
    property alias cfg_showSession: showSessionCheck.checked
    property alias cfg_showWeekly: showWeeklyCheck.checked
    property alias cfg_showExtra: showExtraCheck.checked
    property alias cfg_showPercent: showPercentCheck.checked
    property alias cfg_panelAutoThreshold: thresholdSpin.value
    property string cfg_panelPools: "both"
    property alias cfg_colorLow: lowSwatch.hex
    property alias cfg_colorMid: midSwatch.hex
    property alias cfg_colorHigh: highSwatch.hex
    property alias cfg_colorCritical: criticalSwatch.hex
    property alias cfg_colorEmpty: emptySwatch.hex
    // A ComboBox cannot use `property alias` to currentIndex: the alias would be
    // write-only from Plasma's side at load time, so the saved value never shows.
    property int cfg_leftClickAction: 0
    property string cfg_vendor: ""
    property var cfg_vendorRing: []

    // Which vendors exist and whether they currently work. Sourced from
    // `ai-usagebar usage --json`, which reports every entry enabled in
    // config.toml plus its plan or its error — so you can see that OpenAI has no
    // credentials *before* putting it in the scroll ring.
    property var vendorList: []
    property bool probing: true

    Plasma5Support.DataSource {
        id: prober
        engine: "executable"
        connectedSources: []
        onNewData: (sourceName, data) => {
            disconnectSource(sourceName);
            page.probing = false;
            const report = Logic.parseReport(data["stdout"] || "");
            page.vendorList = report.entries;
        }
    }

    Component.onCompleted: {
        const bin = Logic.shellQuote(page.cfg_binaryPath || Logic.DEFAULT_BINARY);
        prober.connectSource(bin + " usage --json");
    }

    // The report owns the canonical display name, so there is no second table
    // here to drift out of sync with it. Falls back to the raw id while the
    // probe is still running or when the vendor left config.toml.
    function labelFor(id) {
        for (const entry of page.vendorList)
            if (entry.id === id)
                return entry.label;
        return id;
    }

    function ringHas(id) {
        return Array.from(page.cfg_vendorRing || []).indexOf(id) !== -1;
    }

    function setRing(id, on) {
        const next = Array.from(page.cfg_vendorRing || []).filter(v => v !== id);
        if (on)
            next.push(id);
        page.cfg_vendorRing = next;
        // Never leave the applet pointing at a vendor no longer in the ring.
        if (next.length > 0 && next.indexOf(page.cfg_vendor) === -1)
            page.cfg_vendor = next[0];
    }

    Kirigami.FormLayout {
        anchors.fill: parent

        QQC2.Label {
            Kirigami.FormData.label: i18n("Vendors:")
            text: page.probing
                ? i18n("Checking which vendors are configured…")
                : i18n("Tick the ones the scroll gesture cycles through. Status comes from your config.toml.")
            font: Kirigami.Theme.smallFont
            opacity: 0.7
            wrapMode: Text.WordWrap
            Layout.maximumWidth: Kirigami.Units.gridUnit * 24
        }

        Repeater {
            model: page.vendorList

            delegate: RowLayout {
                id: choice
                required property var modelData
                spacing: Kirigami.Units.smallSpacing

                QQC2.CheckBox {
                    text: choice.modelData.label || choice.modelData.id
                    checked: page.ringHas(choice.modelData.id)
                    onToggled: page.setRing(choice.modelData.id, checked)
                }

                // The plan when the vendor answers, its own error when it does
                // not — so "no API key" is visible here, before the vendor goes
                // into the scroll ring, rather than as a ⚠ in the panel later.
                QQC2.Label {
                    readonly property bool failing: choice.modelData.status === "error"
                        || !!choice.modelData.error

                    text: failing ? "✗ " + Logic.errorMessage(choice.modelData.error)
                                  : "✓ " + (choice.modelData.plan || choice.modelData.status)
                    color: failing ? Kirigami.Theme.negativeTextColor
                                   : Kirigami.Theme.positiveTextColor
                    font: Kirigami.Theme.smallFont
                    elide: Text.ElideRight
                    Layout.fillWidth: true
                    Layout.maximumWidth: Kirigami.Units.gridUnit * 20
                }
            }
        }

        QQC2.ComboBox {
            id: currentVendorCombo
            Kirigami.FormData.label: i18n("Current vendor:")
            model: Array.from(page.cfg_vendorRing || [])
            textRole: ""
            displayText: page.labelFor(page.cfg_vendor)
            onActivated: page.cfg_vendor = model[currentIndex]
            delegate: QQC2.ItemDelegate {
                required property var modelData
                width: currentVendorCombo.width
                text: modelData.label
                highlighted: currentVendorCombo.highlightedIndex === index
            }
        }

        Item { Kirigami.FormData.isSection: true }

        QQC2.SpinBox {
            id: intervalSpin
            Kirigami.FormData.label: i18n("Refresh interval (s):")
            // Matches <min>30</min> in config/main.xml and the floor main.qml
            // clamps to. One report covers every configured vendor, so the
            // panel does not need a per-vendor cadence — the countdowns tick
            // locally from reset_at between fetches.
            from: 30
            to: 3600
            stepSize: 30
        }

        QQC2.ComboBox {
            id: clickCombo
            Kirigami.FormData.label: i18n("Left click:")
            model: [i18n("Open the panel popup"), i18n("Open the TUI")]
            currentIndex: page.cfg_leftClickAction
            onActivated: page.cfg_leftClickAction = currentIndex
        }

        QQC2.TextField {
            id: terminalField
            Kirigami.FormData.label: i18n("Terminal:")
            placeholderText: i18n("Automatic (konsole, x-terminal-emulator…)")
        }

        Item { Kirigami.FormData.isSection: true }

        QQC2.CheckBox {
            id: showSessionCheck
            Kirigami.FormData.label: i18n("Display:")
            text: i18n("Show the 5h (session) bar")
        }

        QQC2.CheckBox {
            id: showWeeklyCheck
            text: i18n("Show the weekly bar")
        }

        QQC2.CheckBox {
            id: showExtraCheck
            text: i18n("Show the extra-usage bar ($)")
        }

        QQC2.CheckBox {
            id: showPercentCheck
            text: i18n("Show percentage/value")
        }

        QQC2.CheckBox {
            id: showBarsCheck
            text: i18n("Show bars (off = numbers only)")
        }

        QQC2.CheckBox {
            id: themeColorsCheck
            text: i18n("Follow the Plasma colour scheme")
        }

        QQC2.SpinBox {
            id: barWidthSpin
            Kirigami.FormData.label: i18n("Width of each bar (cells):")
            from: 4
            to: 20
            enabled: showBarsCheck.checked
        }

        QQC2.ComboBox {
            id: poolsCombo
            Kirigami.FormData.label: i18n("Panel pools:")
            model: [i18n("Both"), i18n("First only"), i18n("Second only"), i18n("Automatic")]
            property var ids: ["both", "primary", "secondary", "auto"]
            currentIndex: Math.max(0, ids.indexOf(page.cfg_panelPools))
            onActivated: page.cfg_panelPools = ids[currentIndex]
        }

        QQC2.Label {
            text: i18n("for vendors with two independent pools (e.g. Antigravity)")
            font: Kirigami.Theme.smallFont
            opacity: 0.7
        }

        QQC2.SpinBox {
            id: thresholdSpin
            Kirigami.FormData.label: i18n("Automatic-mode threshold (%):")
            from: 50
            to: 100
            enabled: page.cfg_panelPools === "auto"
        }

        Item { Kirigami.FormData.isSection: true }

        ColorSwatch {
            id: lowSwatch
            Kirigami.FormData.label: i18n("Low (<50%):")
            enabled: !themeColorsCheck.checked
        }

        ColorSwatch {
            id: midSwatch
            Kirigami.FormData.label: i18n("Medium (50-74%):")
            enabled: !themeColorsCheck.checked
        }

        ColorSwatch {
            id: highSwatch
            Kirigami.FormData.label: i18n("High (75-89%):")
            enabled: !themeColorsCheck.checked
        }

        ColorSwatch {
            id: criticalSwatch
            Kirigami.FormData.label: i18n("Critical (>=90%):")
            enabled: !themeColorsCheck.checked
        }

        ColorSwatch {
            id: emptySwatch
            Kirigami.FormData.label: i18n("Empty (bar background):")
            enabled: !themeColorsCheck.checked
        }

        QQC2.Label {
            text: i18n("usadas quando \"Seguir o esquema de cores do Plasma\" está desligado")
            font: Kirigami.Theme.smallFont
            opacity: 0.7
        }

        Item { Kirigami.FormData.isSection: true }

        QQC2.TextField {
            id: binaryField
            Kirigami.FormData.label: i18n("Binary path (empty = auto):")
            placeholderText: i18n("empty = look on PATH")
        }

        QQC2.Label {
            text: i18n("plasmashell does not inherit your shell PATH, so a cargo\ninstall may need the full path here.")
            font: Kirigami.Theme.smallFont
            opacity: 0.7
        }
    }
}
