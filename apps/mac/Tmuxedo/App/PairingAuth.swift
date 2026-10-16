import AppKit
import AuthenticationServices

/// Passkeys, through AuthenticationServices (docs/design/mac-pairing.md §2.3). No Sign in with
/// Apple: Apple does not offer it to Developer ID apps, so the Mac pairs through the iPhone instead.
/// Passkeys: RP `tmuxedo.dev`, which the app may use because the site's
/// apple-app-site-association lists `<team>.dev.tmuxedo.mac` under `webcredentials` and the
/// app carries `webcredentials:tmuxedo.dev`.
@MainActor
final class PairingAuth: NSObject, PairingAuthenticator, ASAuthorizationControllerDelegate,
    ASAuthorizationControllerPresentationContextProviding {
    private var continuation: CheckedContinuation<ASAuthorization, Error>?
    private var controller: ASAuthorizationController?
    /// The relying party: the server's host (tmuxedo.dev in production).
    let rpID: String

    init(rpID: String) { self.rpID = rpID }

    func passkeyAssertion(options: [String: Any]) async throws -> [String: Any] {
        guard let ch = options["challenge"] as? String, let challenge = Data(base64URL: ch) else {
            throw PairingAPI.APIError(status: 0, code: "bad_options", message: "tmuxedo sent passkey options without a challenge.")
        }
        let rp = (options["rpId"] as? String) ?? rpID
        let provider = ASAuthorizationPlatformPublicKeyCredentialProvider(relyingPartyIdentifier: rp)
        let req = provider.createCredentialAssertionRequest(challenge: challenge)
        req.userVerificationPreference = .required
        let allow = (options["allowCredentials"] as? [[String: Any]] ?? []).compactMap { c -> ASAuthorizationPlatformPublicKeyCredentialDescriptor? in
            guard let id = c["id"] as? String, let raw = Data(base64URL: id) else { return nil }
            return ASAuthorizationPlatformPublicKeyCredentialDescriptor(credentialID: raw)
        }
        if !allow.isEmpty { req.allowedCredentials = allow }
        let auth = try await perform([req])
        guard let a = auth.credential as? ASAuthorizationPlatformPublicKeyCredentialAssertion else {
            throw PairingAPI.APIError(status: 0, code: "passkey_failed", message: "The passkey did not answer.")
        }
        var response: [String: Any] = [
            "clientDataJSON": a.rawClientDataJSON.base64URL,
            "authenticatorData": (a.rawAuthenticatorData ?? Data()).base64URL,
            "signature": (a.signature ?? Data()).base64URL,
        ]
        if let u = a.userID, !u.isEmpty { response["userHandle"] = u.base64URL }
        return [
            "id": a.credentialID.base64URL, "rawId": a.credentialID.base64URL, "type": "public-key",
            "response": response, "clientExtensionResults": [String: Any](), "authenticatorAttachment": "platform",
        ]
    }

    private func perform(_ requests: [ASAuthorizationRequest]) async throws -> ASAuthorization {
        try await withCheckedThrowingContinuation { cont in
            continuation = cont
            let c = ASAuthorizationController(authorizationRequests: requests)
            c.delegate = self
            c.presentationContextProvider = self
            controller = c
            c.performRequests()
        }
    }

    nonisolated func authorizationController(controller: ASAuthorizationController, didCompleteWithAuthorization authorization: ASAuthorization) {
        MainActor.assumeIsolated {
            continuation?.resume(returning: authorization)
            continuation = nil
        }
    }

    nonisolated func authorizationController(controller: ASAuthorizationController, didCompleteWithError error: Error) {
        MainActor.assumeIsolated {
            let e = error as NSError
            let mapped: Error = e.domain == ASAuthorizationError.errorDomain && e.code == ASAuthorizationError.canceled.rawValue
                ? PairingAPI.APIError(status: 0, code: "cancelled", message: "Cancelled.") : error
            continuation?.resume(throwing: mapped)
            continuation = nil
        }
    }

    nonisolated func presentationAnchor(for controller: ASAuthorizationController) -> ASPresentationAnchor {
        MainActor.assumeIsolated { NSApp.keyWindow ?? NSApp.windows.first(where: \.isVisible) ?? NSWindow() }
    }
}
