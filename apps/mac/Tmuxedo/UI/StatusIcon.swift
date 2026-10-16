import SwiftUI

/// The menu-bar glyph.
///
/// A SwiftUI view hosted in the status item rather than an NSImage: a symbol
/// effect animates only while asked to (connecting) and costs nothing otherwise.
/// HIT-TEST TRANSPARENT so the status item's own button still gets the click;
/// a hosting view would otherwise swallow it and the menu would never open.
struct StatusIcon: View {
    let display: Display
    var body: some View {
        HStack(spacing: 2) { icon }
            .foregroundStyle(tint)
            .padding(.horizontal, 3)
            .allowsHitTesting(false)
            .accessibilityLabel(display.sentence)
    }

    @ViewBuilder
    private var icon: some View {
        let base = Image(systemName: display.symbol).font(.system(size: 14, weight: .medium))
        if display.animating {
            base.symbolEffect(.variableColor.iterative, options: .repeating)
        } else {
            base
        }
    }

    /// A FAULT IS COLOURED; everything else is template-black, like every menu-bar icon.
    private var tint: Color { display.isFault ? .orange : .primary }
}

/// The coloured dot in the popover's header. Green only for "connected".
struct StateDot: View {
    let display: Display

    var body: some View {
        Circle().fill(colour).frame(width: 8, height: 8).accessibilityHidden(true)
    }

    private var colour: Color {
        switch display {
        case .connected: return .green
        case .connecting, .stopping: return .yellow
        case .unreachable, .broken: return .orange
        case .refused: return .red
        case .off, .notPaired: return .secondary
        }
    }
}
