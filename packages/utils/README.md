# @obsidian-memory/utils

Shared utilities for working with Obsidian vaults in Node.js.

## Features

- **Wiki Link Parsing**: Parse Obsidian-style wiki links from markdown content
- **Path Utilities**: Validate and manipulate file paths safely within a vault

## Installation

```bash
npm install @obsidian-memory/utils
```

## Usage

### Wiki Link Parsing

```typescript
import { parseWikiLinks, extractLinkedNotes } from '@obsidian-memory/utils';

const content = `
# My Note

Check out [[Other Note|this link]].

Also see [[Note#Header]] and ![[Embedded Note]].
`;

// Parse all wiki links with full details
const links = parseWikiLinks(content);
// [
//   { target: 'Other Note', alias: 'this link', isEmbed: false, ... },
//   { target: 'Note', header: 'Header', isEmbed: false, ... },
//   { target: 'Embedded Note', isEmbed: true, ... }
// ]

// Extract unique note names only
const noteNames = extractLinkedNotes(content);
// ['Other Note', 'Note', 'Embedded Note']
```

### Path Utilities

```typescript
import { validatePath, ensureMarkdownExtension, fileExists } from '@obsidian-memory/utils';

// Validate path is within vault (prevents directory traversal)
const safePath = validatePath('/path/to/vault', 'subfolder/note.md');
// '/path/to/vault/subfolder/note.md'

// Ensure .md extension
const notePath = ensureMarkdownExtension('My Note');
// 'My Note.md'

// Check if file exists
const exists = await fileExists('/path/to/file.md');
// true or false
```

## Supported Wiki Link Formats

- Basic links: `[[Note]]`
- Aliases: `[[Note|Display Text]]`
- Headers: `[[Note#Header]]`
- Block references: `[[Note#^block-id]]`
- Embeds: `![[Note]]`

## License

MIT
