import AppKit
import Testing
@testable import RemotexViewer

@MainActor
struct PlainTextEditorTests {
    /// TOML punctuation must reach the file byte-for-byte. These substitutions are
    /// user defaults on macOS, so the editor has to refuse them explicitly.
    @Test
    func configurationDisablesProseSubstitutions() {
        let textView = NSTextView(frame: .zero)

        PlainTextEditor.configure(textView)

        #expect(!textView.isAutomaticQuoteSubstitutionEnabled)
        #expect(!textView.isAutomaticDashSubstitutionEnabled)
        #expect(!textView.isAutomaticTextReplacementEnabled)
        #expect(!textView.isAutomaticSpellingCorrectionEnabled)
        #expect(!textView.smartInsertDeleteEnabled)
        #expect(!textView.isRichText)
    }
}
