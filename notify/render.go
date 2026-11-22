package notify

import (
	"strings"
	"unicode/utf8"
)

const ellipsis = "…"

// TruncateBytes limits s to max bytes without splitting a UTF-8 rune.
//
// WeCom limits bytes rather than characters. Step back to the start of a rune before cutting multibyte text.
//
// A nonpositive max means unlimited length. Add an ellipsis only if it fits.
func TruncateBytes(s string, max int) string {
	if max <= 0 || len(s) <= max {
		return s
	}
	budget := max - len(ellipsis)
	suffix := ellipsis
	if budget < 0 {
		// Skip the ellipsis when it would exceed the byte limit by itself.
		budget = max
		suffix = ""
	}
	cut := budget
	for cut > 0 && !utf8.RuneStart(s[cut]) {
		cut--
	}
	return s[:cut] + suffix
}

// OneLine folds whitespace and truncates by rune count. A nonpositive max leaves length unrestricted.
func OneLine(s string, max int) string {
	s = strings.Join(strings.Fields(s), " ")
	return TruncateRunes(s, max)
}

// TruncateRunes limits s to max runes and adds an ellipsis when truncated. A nonpositive max means unlimited length.
//
// WeCom measures bytes and Telegram measures characters. Using bytes for Telegram reduces capacity for multibyte text.
func TruncateRunes(s string, max int) string {
	if max <= 0 {
		return s
	}
	runes := []rune(s)
	if len(runes) <= max {
		return s
	}
	if max <= 1 {
		return string(runes[:max])
	}
	return string(runes[:max-1]) + ellipsis
}

// TruncateHTML limits an HTML fragment by rune count without leaving a partial tag.
//
// Trim a trailing partial tag after truncation so the destination parser can read the fragment.
//
// Leave tag balancing to Telegram's HTML parser.
func TruncateHTML(s string, max int) string {
	if max <= 0 || len([]rune(s)) <= max {
		return s
	}
	cut := TruncateRunes(s, max)
	// Trim back to the last opening angle bracket when the closing bracket was truncated.
	if lt := strings.LastIndex(cut, "<"); lt >= 0 && !strings.Contains(cut[lt:], ">") {
		cut = cut[:lt]
	}
	// Remove a partial HTML entity as well. A strict parser might reject the entire message otherwise.
	if amp := strings.LastIndex(cut, "&"); amp >= 0 && !strings.Contains(cut[amp:], ";") {
		cut = cut[:amp]
	}
	return cut
}

// packItemCount counts complete items that fit the batch budget.
//
// Items that do not fit remain queued. Use the returned count to mark only included items as delivered.
//
// maxSize sets the budget and a nonpositive value is unlimited. reserve accounts for the heading and footer.
// size measures bytes or runes according to the platform. render supplies the real text for each item.
//
// Return at least one for a nonempty batch so an oversized item cannot block delivery. The caller handles final truncation.
func packItemCount(items []Item, maxSize, reserve int, footer string, size func(string) int, render func(Item, int) string) int {
	if maxSize <= 0 {
		return len(items)
	}
	budget := maxSize - reserve - size(footer)
	if budget < 0 {
		budget = 0
	}
	used := 0
	for i, it := range items {
		used += size(render(it, i))
		if used > budget && i > 0 {
			return i
		}
	}
	return len(items)
}

// byteSize and runeSize measure item sizes in the units used by the channel.
func byteSize(s string) int { return len(s) }
func runeSize(s string) int { return utf8.RuneCountInString(s) }

// assetLine formats up to limit assets and shows the total when names are omitted.
func assetLine(assets []string, limit int) string {
	if len(assets) == 0 {
		return ""
	}
	if limit <= 0 || len(assets) <= limit {
		return strings.Join(assets, "、")
	}
	return strings.Join(assets[:limit], "、") + " 等 " + itoa(len(assets)) + " 个"
}

// itoa abbreviates strconv.Itoa for display formatting.
func itoa(n int) string {
	if n == 0 {
		return "0"
	}
	neg := n < 0
	if neg {
		n = -n
	}
	var buf [20]byte
	i := len(buf)
	for n > 0 {
		i--
		buf[i] = byte('0' + n%10)
		n /= 10
	}
	if neg {
		i--
		buf[i] = '-'
	}
	return string(buf[i:])
}
