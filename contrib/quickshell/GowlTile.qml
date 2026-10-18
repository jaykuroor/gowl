import QtQuick
import QtQuick.Layouts

// Example control tile for the Gowl singleton (Gowl.qml, same directory): a Screen and a Music toggle and one
// status line. Plain Qt Quick with no theme of its own: set the colours/font below or copy the parts you need.
//   click        toggle that mode (if the other one runs, gowl switches over)
//   right-click  pause / resume the running mode
Rectangle {
    id: tile

    property color bg: "#1e1e2e"
    property color surface: "#313244"
    property color fg: "#cdd6f4"
    property color dim: "#7f849c"
    property color accent: "#89b4fa"
    property color bad: "#f38ba8"
    property string fontFamily: ""
    property int fontSize: 13

    implicitWidth: 260
    implicitHeight: column.implicitHeight + 24
    radius: 12
    color: bg

    readonly property string status: {
        if (!Gowl.available) return Gowl.error || "gowl not found"
        if (!Gowl.running) return "Lights: off"
        const what = Gowl.mode === "screen" ? "Screen sync" : Gowl.mode === "music" ? "Music sync"
            : Gowl.mode === "calibrate" ? "Calibrating" : "Syncing"
        const names = Gowl.devices.map(d => d.name).join(", ")
        return what + (Gowl.paused ? " · paused" : "") + (names ? " · " + names : "")
    }

    ColumnLayout {
        id: column
        anchors.fill: parent
        anchors.margins: 12
        spacing: 8

        RowLayout {
            Layout.fillWidth: true
            spacing: 8

            Repeater {
                model: [{ mode: "screen", label: "Screen" }, { mode: "music", label: "Music" }]

                delegate: Rectangle {
                    id: button
                    required property var modelData
                    readonly property bool active: Gowl.running && Gowl.mode === modelData.mode

                    Layout.fillWidth: true
                    implicitHeight: 36
                    radius: 8
                    color: !active ? tile.surface : Gowl.paused ? Qt.darker(tile.accent, 1.8) : tile.accent
                    Behavior on color { ColorAnimation { duration: 120 } }

                    Text {
                        anchors.centerIn: parent
                        text: button.modelData.label
                        color: button.active ? tile.bg : tile.fg
                        font.family: tile.fontFamily
                        font.pixelSize: tile.fontSize
                    }

                    MouseArea {
                        anchors.fill: parent
                        acceptedButtons: Qt.LeftButton | Qt.RightButton
                        cursorShape: Qt.PointingHandCursor
                        onClicked: mouse => {
                            if (mouse.button === Qt.RightButton) {
                                if (button.active) Gowl.paused ? Gowl.resume() : Gowl.pause()
                            } else if (button.modelData.mode === "screen") {
                                Gowl.toggleScreen()
                            } else {
                                Gowl.toggleMusic()
                            }
                        }
                    }
                }
            }
        }

        Text {
            Layout.fillWidth: true
            text: tile.status
            color: tile.dim
            elide: Text.ElideRight
            font.family: tile.fontFamily
            font.pixelSize: tile.fontSize - 1
        }

        Text {
            Layout.fillWidth: true
            visible: Gowl.running && Gowl.error !== ""
            text: Gowl.error
            color: tile.bad
            wrapMode: Text.Wrap
            font.family: tile.fontFamily
            font.pixelSize: tile.fontSize - 1
        }
    }
}
