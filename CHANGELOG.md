# Changelog

## Unreleased

### Fixed
- Honor fixed Windows timezone snapshots, including fully disabled DST, without reloading transition dates from the zone registry (#92).
- Resolve scheduled-event local times on Windows using date-specific native rules; reject DST gaps/folds and failed conversions, preserve historical offsets, and refresh after timezone changes through the existing bounded worker cache (#92).
