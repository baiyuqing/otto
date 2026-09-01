package app

// DynamicContentAvailable is an optional capability frontends can probe before
// attempting operations that would otherwise cross the dynamic-content boundary.
type DynamicContentAvailable interface {
	DynamicContentAvailable() bool
}

func BackendDynamicContentAvailable(backend any) bool {
	available, ok := backend.(DynamicContentAvailable)
	if !ok {
		return true
	}
	return available.DynamicContentAvailable()
}
