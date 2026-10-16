import SwiftUI

/// The runner's log, as a terminal: always dark, monospaced, coloured by what a
/// line IS (LogKind, decided when it is written): red to read, amber to notice,
/// green for connected and paired, cyan for lifecycle, purple for the key.
struct TerminalLogView: View {
    @EnvironmentObject var supervisor: Supervisor
    @EnvironmentObject var settings: Settings

    var body: some View {
        VStack(spacing: 0) {
            titleBar
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 2) {
                        if supervisor.log.isEmpty {
                            Text("# nothing yet: what the runner does appears here").foregroundStyle(Terminal.dim)
                        }
                        ForEach(supervisor.log) { TerminalLine(line: $0) }
                        Color.clear.frame(height: 1).id(Self.bottom)
                    }
                    .font(Terminal.font)
                    .textSelection(.enabled)
                    .padding(12)
                    .frame(maxWidth: .infinity, alignment: .leading)
                }
                // Deferred a turn: scrolling a LazyVStack before layout leaves it scrolled past rows it never draws.
                .onAppear { DispatchQueue.main.async { proxy.scrollTo(Self.bottom, anchor: .bottom) } }
                .onChange(of: supervisor.log.count) { _, _ in
                    withAnimation(.easeOut(duration: 0.15)) { proxy.scrollTo(Self.bottom, anchor: .bottom) }
                }
            }
        }
        .background(Terminal.background)
        .clipShape(RoundedRectangle(cornerRadius: 8))
        .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(Color.white.opacity(0.08)))
        .environment(\.colorScheme, .dark)
    }

    private static let bottom = "terminal-bottom"

    private var titleBar: some View {
        HStack(spacing: 10) {
            Text("runner — \(settings.server?.host ?? "no server")")
                .font(.system(size: 11, weight: .medium, design: .monospaced))
                .foregroundStyle(Terminal.dim)
            Spacer()
            Text("\(supervisor.log.count) line\(supervisor.log.count == 1 ? "" : "s")")
                .font(.system(size: 10, design: .monospaced))
                .foregroundStyle(Terminal.dim)
            Button("Copy") {
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(supervisor.logText, forType: .string)
            }
            Button("Clear") { supervisor.clearLog() }
        }
        .buttonStyle(.borderless)
        .controlSize(.small)
        .foregroundStyle(Terminal.text)
        .padding(.horizontal, 12)
        .padding(.vertical, 7)
        .background(Terminal.bar)
    }
}

struct TerminalLine: View {
    let line: Supervisor.LogLine

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            Text(Terminal.time.string(from: line.at)).foregroundStyle(Terminal.dim.opacity(0.7))
            styled.fixedSize(horizontal: false, vertical: true)
        }
    }

    private var styled: Text {
        switch line.kind {
        case .success: return Text("✔ ").foregroundStyle(Terminal.green) + Text(line.text).foregroundStyle(Terminal.green)
        case .error: return Text("✖ ").foregroundStyle(Terminal.red) + Text(line.text).foregroundStyle(Terminal.red)
        case .warning: return Text("! ").foregroundStyle(Terminal.amber) + Text(line.text).foregroundStyle(Terminal.amber)
        case .lifecycle: return Text("❯ ").foregroundStyle(Terminal.cyan) + Text(line.text).foregroundStyle(Terminal.cyan)
        case .security: return Text("⚿ ").foregroundStyle(Terminal.purple) + Text(line.text).foregroundStyle(Terminal.purple)
        case .info: return Text(line.text).foregroundStyle(Terminal.text.opacity(0.85))
        }
    }
}

extension LogKind {
    var color: Color {
        switch self {
        case .error: return Terminal.red
        case .warning: return Terminal.amber
        case .success: return Terminal.green
        case .lifecycle: return Terminal.cyan
        case .security: return Terminal.purple
        case .info: return Terminal.text.opacity(0.85)
        }
    }
}

/// A GitHub-dark-like palette: readable on near-black and distinct from each other.
enum Terminal {
    static let background = Color(red: 0.051, green: 0.067, blue: 0.090)
    static let bar = Color(red: 0.086, green: 0.106, blue: 0.133)
    static let text = Color(red: 0.788, green: 0.820, blue: 0.851)
    static let dim = Color(red: 0.545, green: 0.580, blue: 0.620)
    static let green = Color(red: 0.247, green: 0.725, blue: 0.314)
    static let red = Color(red: 0.973, green: 0.318, blue: 0.286)
    static let amber = Color(red: 0.824, green: 0.600, blue: 0.133)
    static let cyan = Color(red: 0.337, green: 0.831, blue: 0.867)
    static let purple = Color(red: 0.737, green: 0.549, blue: 1.0)

    static let font = Font.system(size: 11.5, design: .monospaced)

    static let time: DateFormatter = {
        let f = DateFormatter()
        f.dateFormat = "HH:mm:ss"
        return f
    }()
}
