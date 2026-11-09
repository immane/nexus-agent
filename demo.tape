Output docs/images/demo.mp4

# Terminal appearance
Set Width 1440
Set Height 900
Set FontSize 20
Set TypingSpeed 80ms
Set Theme "Dracula"

# 1. Launch Nexus TUI
Type "nexus-tui"
Enter

# Wait for the application to initialize
Sleep 3s

# 2. Press Escape
Escape
Sleep 100ms

# 3. Open the help menu
Type ":help"
Enter

# Keep the help menu visible
Sleep 3s

# 4. Close the help menu
Escape
Sleep 50ms

# 5. Select the hosted demo model
Type ":model set deepseek-v4-1-flash"
Enter
Sleep 500ms

# 6. Return to the main interface
Type "i"
Sleep 50ms

# 7. Send a message
Type "Please summarize @README.md for me."
Enter

# 8. Wait for the Agent response
Sleep 40s
