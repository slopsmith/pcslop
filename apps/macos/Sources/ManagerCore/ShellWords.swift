import Foundation

public struct ShellWordsError: Error, Sendable, Equatable {
    public var message: String
}

public enum ShellWords {
    // POSIX quoting and escapes, but no expansion.
    public static func split(_ text: String) -> Result<[String], ShellWordsError> {
        var words: [String] = []
        var word = ""
        var inWord = false
        var quote: Character?
        var escaped = false
        for ch in text {
            if escaped {
                word.append(ch)
                escaped = false
                continue
            }
            switch (quote, ch) {
            case ("'", "'"), ("\"", "\""):
                quote = nil
            case ("'", _):
                word.append(ch)
            case ("\"", "\\"), (nil, "\\"):
                escaped = true
                inWord = true
            case ("\"", _):
                word.append(ch)
            case (nil, "'"), (nil, "\""):
                quote = ch
                inWord = true
            case (nil, _) where ch.isWhitespace:
                if inWord { words.append(word) }
                word = ""
                inWord = false
            default:
                word.append(ch)
                inWord = true
            }
        }
        if escaped { return .failure(ShellWordsError(message: "a backslash ends the text")) }
        if quote != nil { return .failure(ShellWordsError(message: "a quote is not closed")) }
        if inWord { words.append(word) }
        return .success(words)
    }
}
