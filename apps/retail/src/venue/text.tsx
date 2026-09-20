// Text that can be read on the background it is drawn on, and a field whose typing can be seen.
//
// React Native's `Text` takes no colour from the view around it — unlike the web, where a page's colour cascades
// into everything in it. So a screen themed dark draws its text in the platform's default, which on iOS is black,
// on a background that is also black. The screens were readable in a browser and invisible on a phone, and the
// browser was the accident: `react-native-web` lets the page's colour through.
//
// `ThemeProvider` in `_layout.tsx` does not fix this either. It themes what the navigator draws — headers, the
// screen's own background — and nothing a screen puts inside it.
//
// So these ask the same question `_layout.tsx` asks, and answer it once for every piece of text and every field. A
// `style` passed in still wins, which is what lets a refusal stay red and a confirmation stay green.

import { DarkTheme, DefaultTheme } from 'expo-router';
import {
  Text as Native,
  TextInput as NativeInput,
  useColorScheme,
  type TextInputProps,
  type TextProps,
} from 'react-native';

function scheme() {
  // The same choice `_layout.tsx` makes, so what a screen draws matches what the navigator draws around it.
  // eslint-disable-next-line react-hooks/rules-of-hooks
  return useColorScheme() === 'dark' ? DarkTheme : DefaultTheme;
}

export function Text({ style, ...rest }: TextProps) {
  return <Native style={[{ color: scheme().colors.text }, style]} {...rest} />;
}

export function TextInput({ style, placeholderTextColor, ...rest }: TextInputProps) {
  const theme = scheme();
  return (
    <NativeInput
      style={[{ color: theme.colors.text }, style]}
      // Dimmer than the text, and still legible: a placeholder in the default colour is the same black on the same
      // black, so a field looks empty when it is only unlabelled.
      placeholderTextColor={placeholderTextColor ?? (theme.dark ? '#9a9a9a' : '#6a6a6a')}
      {...rest}
    />
  );
}
