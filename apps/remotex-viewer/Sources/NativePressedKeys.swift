struct NativePressedKeys {
    private var codes = Set<String>()

    mutating func record(code: String, pressed: Bool) {
        if pressed {
            codes.insert(code)
        } else {
            codes.remove(code)
        }
    }

    /// Returns whether a release-all command is necessary, clearing the local
    /// state at the same time so repeated focus notifications stay no-ops.
    mutating func takeForRelease() -> Bool {
        guard !codes.isEmpty else {
            return false
        }
        codes.removeAll()
        return true
    }
}
