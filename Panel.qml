import QtQuick
import QtQuick.Controls
import Quickshell
import Quickshell.Io
import qs.Commons
import qs.Ui

// Bar widget: active Spotify Connect device, with a popup listing every
// Connect device in the account. Clicking one transfers playback there.
// All Spotify logic lives in the `spotify-connect` daemon; this only runs its CLI.
Panel {
  id: root
  moduleName: "io.github.ciryon.spotify-output"
  ipcTarget: "io.github.ciryon.spotify-output"
  manageIpc: false

  readonly property int refreshIntervalSec: Math.max(2, parseInt(String(setting("refreshIntervalSec", 5)), 10) || 5)

  property var devices: []
  property string activeName: ""
  property string lastError: ""
  property int cursor: -1

  readonly property color foreground: bar ? bar.foreground : Color.foreground
  readonly property string fontFamily: bar ? bar.fontFamily : Style.font.family
  readonly property color dim: Qt.darker(foreground, 1.55)
  readonly property string barLabel: activeName !== "" ? "♫ " + activeName : "♫"

  function cli(args) {
    return ["sh", "-c", 'PATH="$PATH:$HOME/.cargo/bin" exec spotify-connect "$@"', "sh"].concat(args)
  }

  function refresh() {
    if (proc.running || switchProc.running) return
    proc.command = cli(["devices", "--json"])
    proc.running = true
  }

  function apply(exitCode, raw) {
    if (exitCode === 2) { devices = []; activeName = ""; lastError = "Spotify unavailable"; return }
    if (exitCode === 3) { devices = []; activeName = ""; lastError = "Not authenticated — run spotify-connect login"; return }
    var list
    try {
      list = JSON.parse(String(raw || ""))
    } catch (e) {
      list = null
    }
    if (exitCode !== 0 || !Array.isArray(list)) { lastError = "Spotify unavailable"; return }
    devices = list
    var active = list.find(function(d) { return d.active })
    activeName = active ? String(active.name) : ""
    lastError = ""
    if (cursor >= list.length) cursor = list.length - 1
  }

  function select(device) {
    if (!device || device.active || switchProc.running) return
    switchProc.command = cli(["switch", String(device.id)])
    switchProc.running = true
    devices = devices.map(function(d) { return Object.assign({}, d, { active: d.id === device.id }) })
    activeName = String(device.name)
    close()
  }

  function moveCursor(dy) {
    if (devices.length === 0) return
    cursor = Math.max(0, Math.min(devices.length - 1, (cursor < 0 ? 0 : cursor + dy)))
  }

  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  onOpenedChanged: if (opened) {
    cursor = -1
    refresh()
    Qt.callLater(function() { keyCatcher.forceActiveFocus() })
  }

  Timer {
    interval: root.opened ? 1000 : root.refreshIntervalSec * 1000
    repeat: true
    running: true
    triggeredOnStart: true
    onTriggered: root.refresh()
  }

  Process {
    id: proc
    running: false
    command: []
    stdout: StdioCollector { id: out; waitForEnd: true }
    onExited: function(exitCode) { root.apply(exitCode, out.text) }
  }

  Process {
    id: switchProc
    running: false
    command: []
    onExited: function(exitCode) {
      if (exitCode !== 0) root.lastError = "Could not switch output"
      root.refresh()
    }
  }

  IpcHandler {
    target: root.ipcTarget
    function open(): void { root.open() }
    function close(): void { root.close() }
    function show(): void { root.open() }
    function hide(): void { root.close() }
    function toggle(): void { root.toggle() }
    function refresh(): string { root.refresh(); return "ok" }
    function active(): string { return root.activeName }
  }

  WidgetButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    text: root.barLabel
    tooltipText: root.lastError !== "" ? root.lastError : "Spotify Connect: " + (root.activeName || "nothing playing")
    onPressed: function(buttonCode) {
      if (buttonCode === Qt.RightButton) root.refresh()
      else root.toggle()
    }
  }

  KeyboardPanel {
    id: panel
    anchorItem: button
    owner: root
    bar: root.bar
    open: root.opened
    focusTarget: keyCatcher
    contentWidth: panel.fittedContentWidth(Style.space(320))
    contentHeight: panel.fittedContentHeight(column.implicitHeight, Style.space(480))

    PanelKeyCatcher {
      id: keyCatcher
      anchors.fill: parent
      onMoveRequested: function(dx, dy) { root.moveCursor(dy) }
      onActivateRequested: if (root.cursor >= 0) root.select(root.devices[root.cursor])
      onCloseRequested: root.close()
      onTabRequested: function(direction) { root.switchPanel(direction) }
      onTextKey: function(t) { if (t === "r" || t === "R") root.refresh() }

      Flickable {
        id: flick
        anchors.fill: parent
        contentWidth: width
        contentHeight: column.implicitHeight
        clip: true
        boundsBehavior: Flickable.StopAtBounds
        flickableDirection: Flickable.VerticalFlick
        interactive: contentHeight > height
        ScrollBar.vertical: ScrollBar { policy: ScrollBar.AsNeeded }

        Column {
          id: column
          width: flick.width
          spacing: Style.space(4)

          PanelHero {
            width: parent.width
            title: "Spotify Connect"
            meta: root.lastError !== "" ? root.lastError : (root.activeName || "Nothing playing")
            foreground: root.foreground
            fontFamily: root.fontFamily
            iconComponent: Component {
              Text {
                text: "♫"
                color: root.foreground
                font.family: root.fontFamily
                font.pixelSize: Style.font.display
              }
            }
          }

          Text {
            visible: root.devices.length === 0 && root.lastError === ""
            width: parent.width
            text: "No devices"
            color: root.dim
            font.family: root.fontFamily
            font.pixelSize: Style.font.body
            topPadding: Style.space(8)
          }

          Repeater {
            model: root.devices

            CursorSurface {
              width: column.width
              implicitHeight: rowText.implicitHeight + Style.space(12)
              hasCursor: root.cursor === index
              current: modelData.active
              foreground: root.foreground

              Text {
                id: rowText
                anchors.verticalCenter: parent.verticalCenter
                anchors.left: parent.left
                anchors.right: parent.right
                anchors.leftMargin: Style.space(8)
                anchors.rightMargin: Style.space(8)
                text: (modelData.active ? "● " : "○ ") + modelData.name
                color: root.foreground
                font.family: root.fontFamily
                font.pixelSize: Style.font.body
                font.bold: modelData.active
                elide: Text.ElideRight
              }

              HoverHandler { onHoveredChanged: if (hovered) root.cursor = index }

              TapHandler { onTapped: root.select(modelData) }
            }
          }
        }
      }
    }
  }
}
