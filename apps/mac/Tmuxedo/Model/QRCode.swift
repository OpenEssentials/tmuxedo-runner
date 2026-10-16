import CoreImage
import CoreImage.CIFilterBuiltins
import Foundation

/// The pairing QR code: the attempt's universal link (`/m/pair?code=…`), which opens the
/// tmuxedo app on a phone that has it and the approve page in the browser otherwise.
enum QRCode {
    /// A crisp, scaled image (nearest-neighbour), or nil for an empty payload.
    static func image(for text: String, scale: CGFloat = 8) -> CGImage? {
        guard !text.isEmpty else { return nil }
        let f = CIFilter.qrCodeGenerator()
        f.message = Data(text.utf8)
        f.correctionLevel = "M"
        guard let out = f.outputImage?.transformed(by: CGAffineTransform(scaleX: scale, y: scale)) else { return nil }
        return CIContext(options: [.useSoftwareRenderer: true]).createCGImage(out, from: out.extent)
    }
}
