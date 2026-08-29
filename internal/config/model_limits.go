package config

import (
	"regexp"
	"strings"
)

type ModelLimits struct {
	Known           bool
	ContextWindow   int
	HardInputWindow int
	MaxOutputTokens int
	WorkingWindow   int
	SourceURL       string
}

type modelFamily uint8

const (
	unknownModelFamily modelFamily = iota
	gptLongFamily
	gpt400KFamily
	gptChatFamily
	gptSparkFamily
	gpt41Family
	gpt4oFamily
	gpt4o4096Family
	oSeriesFamily
	claudeMillion128Family
	claudeMillion64Family
	claude200K64Family
	claude200K32Family
	claude200K8192Family
	claude200K4096Family
)

const (
	openAIModelSourceURL    = "https://developers.openai.com/api/docs/models/all"
	anthropicModelSourceURL = "https://platform.claude.com/docs/en/models/overview"
)

var openAIModelAliases = map[string]modelFamily{
	"gpt-5.6":             gptLongFamily,
	"gpt-5.6-sol":         gptLongFamily,
	"gpt-5.6-terra":       gptLongFamily,
	"gpt-5.6-luna":        gptLongFamily,
	"gpt-5.5":             gptLongFamily,
	"gpt-5.5-pro":         gptLongFamily,
	"gpt-5.4":             gptLongFamily,
	"gpt-5.4-pro":         gptLongFamily,
	"gpt-5.4-mini":        gpt400KFamily,
	"gpt-5.4-nano":        gpt400KFamily,
	"gpt-5.3-chat-latest": gptChatFamily,
	"gpt-5.3-codex":       gpt400KFamily,
	"gpt-5.3-codex-spark": gptSparkFamily,
	"gpt-5.2":             gpt400KFamily,
	"gpt-5.2-pro":         gpt400KFamily,
	"gpt-5.2-codex":       gpt400KFamily,
	"gpt-5.2-chat-latest": gptChatFamily,
	"gpt-5.1":             gpt400KFamily,
	"gpt-5.1-codex":       gpt400KFamily,
	"gpt-5.1-codex-mini":  gpt400KFamily,
	"gpt-5.1-codex-max":   gpt400KFamily,
	"gpt-5.1-chat-latest": gptChatFamily,
	"gpt-5":               gpt400KFamily,
	"gpt-5-pro":           gpt400KFamily,
	"gpt-5-mini":          gpt400KFamily,
	"gpt-5-nano":          gpt400KFamily,
	"gpt-5-codex":         gpt400KFamily,
	"gpt-5-chat-latest":   gptChatFamily,
	"gpt-4.1":             gpt41Family,
	"gpt-4.1-mini":        gpt41Family,
	"gpt-4.1-nano":        gpt41Family,
	"gpt-4o":              gpt4oFamily,
	"gpt-4o-mini":         gpt4oFamily,
	"o1":                  oSeriesFamily,
	"o1-pro":              oSeriesFamily,
	"o3":                  oSeriesFamily,
	"o3-mini":             oSeriesFamily,
	"o3-pro":              oSeriesFamily,
	"o4-mini":             oSeriesFamily,
}

var anthropicModelAliases = map[string]modelFamily{
	"claude-fable-5":    claudeMillion128Family,
	"claude-opus-5":     claudeMillion128Family,
	"claude-sonnet-5":   claudeMillion128Family,
	"claude-opus-4-8":   claudeMillion128Family,
	"claude-opus-4.8":   claudeMillion128Family,
	"claude-opus-4-7":   claudeMillion128Family,
	"claude-opus-4.7":   claudeMillion128Family,
	"claude-opus-4-6":   claudeMillion128Family,
	"claude-opus-4.6":   claudeMillion128Family,
	"claude-opus-4-5":   claude200K64Family,
	"claude-opus-4.5":   claude200K64Family,
	"claude-opus-4-1":   claude200K32Family,
	"claude-opus-4.1":   claude200K32Family,
	"claude-opus-4":     claude200K32Family,
	"claude-sonnet-4-6": claudeMillion128Family,
	"claude-sonnet-4.6": claudeMillion128Family,
	"claude-sonnet-4-5": claudeMillion64Family,
	"claude-sonnet-4.5": claudeMillion64Family,
	"claude-sonnet-4":   claude200K64Family,
	"claude-3-7-sonnet": claude200K64Family,
	"claude-3.7-sonnet": claude200K64Family,
	"claude-3-5-sonnet": claude200K8192Family,
	"claude-3.5-sonnet": claude200K8192Family,
	"claude-haiku-4-5":  claude200K64Family,
	"claude-haiku-4.5":  claude200K64Family,
	"claude-3-5-haiku":  claude200K8192Family,
	"claude-3.5-haiku":  claude200K8192Family,
	"claude-3-haiku":    claude200K4096Family,
}

var exactModelSnapshots = map[string]modelFamily{
	"gpt-4o-2024-05-13":      gpt4o4096Family,
	"gpt-4o-2024-08-06":      gpt4oFamily,
	"gpt-4o-2024-11-20":      gpt4oFamily,
	"gpt-4o-mini-2024-07-18": gpt4oFamily,
}

var (
	openAIDateSuffix    = regexp.MustCompile(`^(.*)-[0-9]{4}-[0-9]{2}-[0-9]{2}$`)
	anthropicDateSuffix = regexp.MustCompile(`^(.*)-[0-9]{8}$`)
)

type modelNamespace uint8

const (
	modelNamespaceNone modelNamespace = iota
	modelNamespaceOpenAI
	modelNamespaceAnthropic
)

func resolveModelLimits(model string) ModelLimits {
	model, namespace, ok := unwrapModelID(model)
	if !ok {
		return ModelLimits{}
	}

	if namespace != modelNamespaceAnthropic {
		if family, found := exactModelSnapshots[model]; found {
			return limitsForFamily(family)
		}
		if family, found := openAIModelAliases[model]; found {
			return limitsForFamily(family)
		}
		if match := openAIDateSuffix.FindStringSubmatch(model); match != nil {
			if family, found := openAIModelAliases[match[1]]; found {
				return limitsForFamily(family)
			}
		}
	}

	if namespace != modelNamespaceOpenAI {
		if family, found := anthropicModelAliases[model]; found {
			return limitsForFamily(family)
		}
		if match := anthropicDateSuffix.FindStringSubmatch(model); match != nil {
			if family, found := anthropicModelAliases[match[1]]; found {
				return limitsForFamily(family)
			}
		}
	}

	return ModelLimits{}
}

func unwrapModelID(model string) (string, modelNamespace, bool) {
	model = strings.TrimSuffix(model, ":batch")
	if strings.Contains(model, ":batch") {
		return "", modelNamespaceNone, false
	}

	namespace := modelNamespaceNone
	switch {
	case strings.HasPrefix(model, "openai/"):
		model = strings.TrimPrefix(model, "openai/")
		namespace = modelNamespaceOpenAI
	case strings.HasPrefix(model, "anthropic/"):
		model = strings.TrimPrefix(model, "anthropic/")
		namespace = modelNamespaceAnthropic
	}
	if model == "" || strings.Contains(model, "/") {
		return "", modelNamespaceNone, false
	}
	return model, namespace, true
}

func limitsForFamily(family modelFamily) ModelLimits {
	switch family {
	case gptLongFamily:
		return knownModelLimits(1_050_000, 922_000, 272_000, 128_000, openAIModelSourceURL)
	case gpt400KFamily:
		return knownModelLimits(400_000, 272_000, 272_000, 128_000, openAIModelSourceURL)
	case gptChatFamily:
		return knownModelLimits(128_000, 128_000, 128_000, 16_384, openAIModelSourceURL)
	case gptSparkFamily:
		return knownModelLimits(128_000, 128_000, 128_000, 32_000, openAIModelSourceURL)
	case gpt41Family:
		return knownModelLimits(1_047_576, 1_047_576, 1_047_576, 32_768, openAIModelSourceURL)
	case gpt4oFamily:
		return knownModelLimits(128_000, 128_000, 128_000, 16_384, openAIModelSourceURL)
	case gpt4o4096Family:
		return knownModelLimits(128_000, 128_000, 128_000, 4_096, openAIModelSourceURL)
	case oSeriesFamily:
		return knownModelLimits(200_000, 200_000, 200_000, 100_000, openAIModelSourceURL)
	case claudeMillion128Family:
		return knownModelLimits(1_000_000, 1_000_000, 1_000_000, 128_000, anthropicModelSourceURL)
	case claudeMillion64Family:
		return knownModelLimits(1_000_000, 1_000_000, 1_000_000, 64_000, anthropicModelSourceURL)
	case claude200K64Family:
		return knownModelLimits(200_000, 200_000, 200_000, 64_000, anthropicModelSourceURL)
	case claude200K32Family:
		return knownModelLimits(200_000, 200_000, 200_000, 32_000, anthropicModelSourceURL)
	case claude200K8192Family:
		return knownModelLimits(200_000, 200_000, 200_000, 8_192, anthropicModelSourceURL)
	case claude200K4096Family:
		return knownModelLimits(200_000, 200_000, 200_000, 4_096, anthropicModelSourceURL)
	default:
		return ModelLimits{}
	}
}

func knownModelLimits(context, hardInput, working, maxOutput int, sourceURL string) ModelLimits {
	return ModelLimits{
		Known:           true,
		ContextWindow:   context,
		HardInputWindow: hardInput,
		MaxOutputTokens: maxOutput,
		WorkingWindow:   working,
		SourceURL:       sourceURL,
	}
}
