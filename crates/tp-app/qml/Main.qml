import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import QtMultimedia
import TelePresence

ApplicationWindow {
    id: win
    width: 1040
    height: 700
    visible: true
    title: "TelePresence"
    color: "#1e1e2e"

    readonly property color fg: "#cdd6f4"
    readonly property color dim: "#9399b2"
    readonly property color accent: "#89b4fa"
    readonly property color panel: "#313244"
    readonly property color bad: "#f38ba8"

    Controller { id: ctl }

    property string selectedRole: "receiver"
    readonly property bool isSender: selectedRole === "sender"
    readonly property var peers: JSON.parse(ctl.peers || "[]")
    readonly property var cameras: JSON.parse(ctl.cameras || "[]")
    readonly property var mics: JSON.parse(ctl.mics || "[]")
    readonly property var outputs: JSON.parse(ctl.outputs || "[]")
    readonly property var modes: JSON.parse(ctl.modes || "[]")
    readonly property var paired: JSON.parse(ctl.paired || "[]")
    readonly property var saved: JSON.parse(ctl.saved || "{}")
    property string chosenPeer: ""
    // The receiver a Connect would reach, when discovery knows it.
    readonly property var targetPeer: manualPeer.text.trim() !== "" ? null
                                      : (peers.find(p => p.addr === chosenPeer) || null)
    // Pairing state is unknown for a typed address or a receiver discovery
    // has not seen, so the code field shows with a hint to leave it blank.
    readonly property bool targetUnknown: targetPeer === null
    readonly property bool showCode: ctl.needsCode || targetUnknown || !targetPeer.pinned
    // Set once the saved settings are on screen, so restoring them does not
    // count as the user changing something.
    property bool restored: false

    // Fixed choices ahead of the device names in the audio output picker.
    readonly property var outputFixed: [
        { label: "Auto (VB-Cable if installed)", value: "auto" },
        { label: "System default output", value: "default" },
        { label: "Nowhere", value: "none" }
    ]
    readonly property var outputChoices: outputFixed.concat(outputs.map(n => ({ label: n, value: n })))
    readonly property var modeChoices: [{ label: "Native (auto)", value: "native" }].concat(
        modes.map(m => ({ label: m.label, value: m.w + "x" + m.h + "@" + m.fps })))

    Component.onCompleted: {
        ctl.init()
        restore()
    }

    function indexOfValue(list, key, value) {
        for (let i = 0; i < list.length; i++)
            if (list[i][key] === value) return i
        return -1
    }

    function restore() {
        const s = win.saved
        const r = s.receiver || {}
        const snd = s.sender || {}
        selectedRole = s.role === "sender" ? "sender" : "receiver"
        if (r.port) portBox.value = r.port
        if (r.name) nameField.text = r.name
        autoBitrate.checked = !snd.bitrate_kbps
        if (snd.bitrate_kbps) bitrateBox.value = snd.bitrate_kbps
        if (snd.last_peer) chosenPeer = snd.last_peer
        restored = true
        // Come up in the state the last session left: a receiver listens
        // straight away, a sender reconnects to the receiver it last reached.
        if (selectedRole === "receiver")
            start()
        else if (snd.last_peer)
            start()
    }

    // Device lists arrive asynchronously; select the saved ids when they do.
    onCamerasChanged: {
        const i = indexOfValue(cameras, "id", (saved.sender || {}).camera)
        cameraBox.currentIndex = i >= 0 ? i : (cameras.length > 0 ? 0 : -1)
        if (cameraBox.currentIndex >= 0) ctl.loadModes(cameras[cameraBox.currentIndex].id)
    }
    onMicsChanged: {
        const i = indexOfValue(mics, "id", (saved.sender || {}).mic)
        micBox.currentIndex = i
    }
    onModesChanged: {
        const snd = saved.sender || {}
        let v = "native"
        if (snd.size) v = snd.size[0] + "x" + snd.size[1] + "@" + (snd.fps || 0)
        const i = indexOfValue(modeChoices, "value", v)
        modeBox.currentIndex = i >= 0 ? i : 0
    }
    onOutputChoicesChanged: {
        const v = (saved.receiver || {}).audio_out || "auto"
        const i = indexOfValue(outputChoices, "value", v)
        outputBox.currentIndex = i >= 0 ? i : 0
    }

    function start() {
        if (isSender) {
            const target = manualPeer.text.trim() !== "" ? manualPeer.text.trim() : chosenPeer
            ctl.startSender(target, win.showCode ? codeField.text : "",
                            cameraBox.currentIndex >= 0 ? cameras[cameraBox.currentIndex].id : "",
                            micBox.currentIndex >= 0 ? mics[micBox.currentIndex].id : "",
                            modeBox.currentIndex >= 0 ? modeChoices[modeBox.currentIndex].value : "native",
                            autoBitrate.checked ? 0 : bitrateBox.value,
                            testSource.checked)
            codeField.text = ""
        } else {
            ctl.startReceiver(portBox.value, nameField.text,
                              outputBox.currentIndex >= 0 ? outputChoices[outputBox.currentIndex].value : "auto")
        }
    }

    // Settings apply live: a change to the running role restarts its session.
    function changed(role) {
        if (restored && ctl.running && ctl.role === role) start()
    }

    component Label2: Label { color: win.fg }
    component Caption: Label { color: win.dim; font.pixelSize: 12 }

    header: ToolBar {
        background: Rectangle { color: win.panel }
        RowLayout {
            anchors.fill: parent
            anchors.margins: 6
            spacing: 8
            TabBar {
                currentIndex: win.isSender ? 1 : 0
                TabButton {
                    text: "Receiver"; width: 110
                    onClicked: { win.selectedRole = "receiver"; ctl.setSavedRole("receiver") }
                }
                TabButton {
                    text: "Sender"; width: 110
                    onClicked: { win.selectedRole = "sender"; ctl.setSavedRole("sender") }
                }
            }
            Item { Layout.fillWidth: true }
            Label2 { text: ctl.running ? ("Running as " + ctl.role) : "Stopped" }
            Button {
                text: ctl.running && ctl.role === win.selectedRole
                      ? "Stop"
                      : (win.isSender ? "Connect" : "Start listening")
                highlighted: true
                onClicked: (ctl.running && ctl.role === win.selectedRole) ? ctl.stop() : win.start()
            }
        }
    }

    ColumnLayout {
        anchors.fill: parent
        anchors.margins: 12
        spacing: 10

        // Error banner: what went wrong and, from the core, what to do.
        Rectangle {
            Layout.fillWidth: true
            visible: ctl.error !== ""
            implicitHeight: errText.implicitHeight + 16
            color: "#45273a"
            border.color: win.bad
            radius: 6
            RowLayout {
                anchors.fill: parent
                anchors.margins: 8
                Label {
                    id: errText
                    Layout.fillWidth: true
                    text: ctl.error
                    color: win.bad
                    wrapMode: Text.Wrap
                }
                Button { text: "Refresh devices"; visible: win.isSender; onClicked: ctl.refreshDevices() }
            }
        }

        RowLayout {
            Layout.fillWidth: true
            Layout.fillHeight: true
            spacing: 12

            Rectangle {
                Layout.preferredWidth: 360
                Layout.fillHeight: true
                color: win.panel
                radius: 8

                ScrollView {
                    id: settingsScroll
                    anchors.fill: parent
                    anchors.margins: 12
                    clip: true
                    contentWidth: availableWidth

                    ColumnLayout {
                        width: settingsScroll.availableWidth
                        spacing: 6

                        // Receiver settings
                        ColumnLayout {
                            visible: !win.isSender
                            Layout.fillWidth: true
                            spacing: 6
                            Caption { text: "Name shown to senders" }
                            TextField {
                                id: nameField
                                Layout.fillWidth: true
                                placeholderText: "This computer's name"
                                onEditingFinished: win.changed("receiver")
                            }
                            Caption { text: "UDP port" }
                            SpinBox {
                                id: portBox; from: 1024; to: 65535; value: 47800; editable: true
                                onValueModified: win.changed("receiver")
                            }
                            Caption { text: "Play received audio into" }
                            ComboBox {
                                id: outputBox
                                Layout.fillWidth: true
                                model: win.outputChoices
                                textRole: "label"
                                onActivated: win.changed("receiver")
                            }
                            Caption { text: "Paired senders"; Layout.topMargin: 10 }
                            Repeater {
                                model: win.paired
                                delegate: RowLayout {
                                    required property var modelData
                                    Layout.fillWidth: true
                                    Label2 { text: modelData.name; Layout.fillWidth: true; elide: Text.ElideRight }
                                    Button {
                                        text: "Forget"
                                        onClicked: {
                                            ctl.forgetSender(modelData.fingerprint)
                                            // The running endpoint keeps its own pin list.
                                            win.changed("receiver")
                                        }
                                    }
                                }
                            }
                            Caption { visible: win.paired.length === 0; text: "None yet" }
                        }

                        // Sender settings
                        ColumnLayout {
                            visible: win.isSender
                            Layout.fillWidth: true
                            spacing: 6
                            Caption { text: "Receivers on this network" }
                            Rectangle {
                                Layout.fillWidth: true
                                Layout.preferredHeight: 120
                                color: "#181825"
                                radius: 6
                                ListView {
                                    id: peerList
                                    anchors.fill: parent
                                    anchors.margins: 4
                                    clip: true
                                    model: win.peers
                                    SmoothWheel { flick: peerList }
                                    delegate: ItemDelegate {
                                        required property var modelData
                                        width: peerList.width
                                        highlighted: win.chosenPeer === modelData.addr
                                        onClicked: { win.chosenPeer = modelData.addr; manualPeer.text = "" }
                                        contentItem: Column {
                                            Label2 { text: modelData.name + (modelData.pinned ? "  (paired)" : "  (not paired)") }
                                            Caption { text: modelData.addr }
                                        }
                                    }
                                    Caption {
                                        anchors.centerIn: parent
                                        visible: peerList.count === 0
                                        text: "Searching..."
                                    }
                                }
                            }
                            Caption { text: "Or address (ip or ip:port)" }
                            TextField { id: manualPeer; Layout.fillWidth: true; placeholderText: win.chosenPeer || "192.168.1.20:47800" }

                            ColumnLayout {
                                visible: win.showCode
                                Layout.fillWidth: true
                                Caption {
                                    text: "Pairing code (shown on the receiver)"
                                    color: ctl.needsCode ? win.bad : win.dim
                                }
                                TextField {
                                    id: codeField
                                    Layout.fillWidth: true
                                    placeholderText: win.targetUnknown && !ctl.needsCode
                                                     ? "Leave blank if already paired" : "123456"
                                    maximumLength: 6
                                    inputMethodHints: Qt.ImhDigitsOnly
                                    validator: RegularExpressionValidator { regularExpression: /[0-9]{0,6}/ }
                                    onAccepted: win.start()
                                }
                                Caption {
                                    visible: win.targetUnknown && !ctl.needsCode
                                    text: "Leave blank if this sender is already paired with it."
                                }
                            }

                            Caption { text: "Camera"; Layout.topMargin: 8 }
                            ComboBox {
                                id: cameraBox
                                Layout.fillWidth: true
                                model: win.cameras
                                textRole: "name"
                                onActivated: {
                                    ctl.loadModes(win.cameras[currentIndex].id)
                                    win.changed("sender")
                                }
                            }
                            Caption { text: "Resolution" }
                            ComboBox {
                                id: modeBox
                                Layout.fillWidth: true
                                model: win.modeChoices
                                textRole: "label"
                                onActivated: win.changed("sender")
                            }
                            Caption { text: "Microphone" }
                            ComboBox {
                                id: micBox
                                Layout.fillWidth: true
                                model: win.mics
                                textRole: "name"
                                displayText: currentIndex >= 0 ? currentText : "Saved or built-in"
                                onActivated: win.changed("sender")
                                hoverEnabled: true
                                ToolTip.visible: hovered
                                ToolTip.delay: 400
                                ToolTip.text: "There is no \"Default\" choice on purpose: the system default input "
                                              + "follows whatever connected last, so AirPods would take over the call's "
                                              + "microphone. The device picked here stays until you change it."
                            }
                            Caption { text: "Video bitrate" }
                            RowLayout {
                                CheckBox {
                                    id: autoBitrate
                                    text: "Auto"
                                    checked: true
                                    palette.windowText: win.fg
                                    onToggled: win.changed("sender")
                                }
                                SpinBox {
                                    id: bitrateBox
                                    enabled: !autoBitrate.checked
                                    from: 500; to: 40000; stepSize: 500; value: 6000; editable: true
                                    onValueModified: win.changed("sender")
                                }
                                Caption { text: "kbit/s" }
                            }
                            CheckBox {
                                id: testSource
                                text: "Test pattern and tone"
                                palette.windowText: win.fg
                                onToggled: win.changed("sender")
                            }
                            Button { text: "Refresh devices"; onClicked: ctl.refreshDevices() }
                        }
                    }
                }
            }

            ColumnLayout {
                Layout.fillWidth: true
                Layout.fillHeight: true
                spacing: 10

                Rectangle {
                    Layout.fillWidth: true
                    Layout.fillHeight: true
                    color: "#11111b"
                    radius: 8
                    VideoOutput {
                        id: videoOut
                        anchors.fill: parent
                        anchors.margins: 4
                        fillMode: VideoOutput.PreserveAspectFit
                        visible: ctl.role === "receiver" && ctl.videoFormat !== ""
                        Component.onCompleted: ctl.attachVideoSink(videoSink)
                    }
                    Caption {
                        anchors.centerIn: parent
                        visible: !videoOut.visible && ctl.pairingCode === ""
                        text: ctl.running && ctl.role === "sender"
                              ? "Streaming. Local preview is not shown on the sender."
                              : "No video"
                    }
                    // A sender wants to pair: the code is the one thing to see.
                    Column {
                        anchors.centerIn: parent
                        visible: ctl.pairingCode !== "" && ctl.role === "receiver"
                        spacing: 8
                        Caption { anchors.horizontalCenter: parent.horizontalCenter; text: "Pairing code" }
                        Label {
                            anchors.horizontalCenter: parent.horizontalCenter
                            text: ctl.pairingCode
                            color: win.accent
                            font.pixelSize: 72
                            font.letterSpacing: 10
                            font.family: "Consolas"
                        }
                        Caption {
                            anchors.horizontalCenter: parent.horizontalCenter
                            text: "Enter this on the sender. It works once."
                        }
                    }
                }

                Rectangle {
                    Layout.fillWidth: true
                    Layout.preferredHeight: 140
                    color: win.panel
                    radius: 8
                    GridLayout {
                        anchors.fill: parent
                        anchors.margins: 12
                        columns: 4
                        columnSpacing: 18
                        rowSpacing: 4
                        Caption { text: "Status" }
                        Label2 { text: ctl.status; Layout.columnSpan: 3; Layout.fillWidth: true; elide: Text.ElideRight }
                        Caption { text: "Peer" }
                        Label2 { text: ctl.peer !== "" ? ctl.peer : "none"; Layout.columnSpan: 3; Layout.fillWidth: true; elide: Text.ElideRight }
                        Caption { text: "Format" }
                        Label2 { text: ctl.videoFormat !== "" ? ctl.videoFormat : "-"; Layout.columnSpan: 3 }
                        Caption { text: "Video" }
                        Label2 {
                            text: ctl.fps.toFixed(1) + " fps   " + Math.round(ctl.kbps) + " kbit/s"
                                  + (ctl.targetKbps > 0 ? "   (target " + ctl.targetKbps + ")" : "")
                        }
                        Caption { text: "Link" }
                        Label2 { text: (ctl.loss * 100).toFixed(2) + "% loss   " + ctl.rtt + " ms" }
                        Caption { text: ctl.role === "sender" ? "Mic" : "Audio" }
                        Rectangle {
                            Layout.columnSpan: 3
                            Layout.fillWidth: true
                            height: 10
                            radius: 5
                            color: "#181825"
                            Rectangle {
                                height: parent.height
                                radius: 5
                                width: parent.width * Math.min(1, ctl.level)
                                color: ctl.level > 0.9 ? win.bad : "#a6e3a1"
                                Behavior on width { NumberAnimation { duration: 60 } }
                            }
                        }
                    }
                }
            }
        }
    }
}
