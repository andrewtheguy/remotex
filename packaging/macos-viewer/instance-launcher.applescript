-- A double-clickable launcher for one remotex instance.
--
-- Compile it into an app of its own, with its own name and icon:
--
--   osacompile -o ~/Applications/remotex\ Work.app \
--     packaging/macos-viewer/instance-launcher.applescript
--
-- Two things make this necessary rather than decorative. Double-clicking remotex.app
-- passes no arguments — LaunchServices does not — so `--instance-dir` has nowhere to
-- come from; and `open` without `-n` reactivates the running copy and silently discards
-- `--args`, which is the same trap the Chrome `--user-data-dir` launchers hit.
--
-- Edit the two values below. See docs/macos-viewer.md, "Running more than one
-- instance", for the rest of the flow: the branding line, the icon, and the `rxa` key
-- each new instance mints.
on run
	set appPath to "/Applications/remotex.app"
	set instanceName to "remotex-work"

	-- Built from `path to home folder` rather than "$HOME": `do shell script` does not
	-- expand shell variables inside the quoting below. An absolute path works too.
	set instanceDir to (POSIX path of (path to home folder)) & "Library/Application Support/" & instanceName
	do shell script "/usr/bin/open -n " & quoted form of appPath & ¬
		" --args --instance-dir " & quoted form of instanceDir
end run
