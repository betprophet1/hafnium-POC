// Parlays: picking several markets that must all go your way, and asking what that costs.
//
// A parlay pays $1 if every leg resolves as picked and nothing otherwise, so it cannot be priced off any one book —
// there is no book until somebody asks. Asking is what this screen does: it sends the legs, the venue lists a market
// for that exact combination, and approved responders answer with firm quotes, which are real sell orders resting on
// the parlay's own book. Taking one is an ordinary buy at that price (interfaces/gateway/README.md, parlays).
//
// So nothing here invents a price, exactly as the market screen does not: the app sends legs and a quantity, and
// every number it shows afterwards came back from the venue.
//
// Quotes also arrive on the private stream, but this screen asks for them instead. The stream in this app only logs
// in and heartbeats, and a request lives for about thirty seconds — asking twice a second for that long is a cheaper
// thing to get right than a send path, and it is what the market screen already does with the book.

import { useCallback, useEffect, useMemo, useState } from 'react';
import { ActivityIndicator, Pressable, RefreshControl, ScrollView, StyleSheet, View } from 'react-native';

import {
  LOT_STEP,
  MAX_LEGS,
  MIN_LEGS,
  byEvent,
  filledContracts,
  legsOk,
  money,
  offerable,
  payoutCents,
  quantityOk,
  secondsUntil,
  type Pick,
} from '@/venue/parlay';
import { useVenue } from '@/venue/session';
import { Text, TextInput } from '@/venue/text';
import type { Market, OrderAnswer, Outcome, Quote, RfqStatus } from '@/venue/types';

export default function ParlaysScreen() {
  const { venue, member, refresh } = useVenue();
  const [markets, setMarkets] = useState<Market[] | null>(null);
  const [picks, setPicks] = useState<Map<string, Pick>>(new Map());
  const [quantity, setQuantity] = useState(String(LOT_STEP));
  const [rfq, setRfq] = useState<RfqStatus | null>(null);
  const [failed, setFailed] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const [sending, setSending] = useState(false);
  const [taking, setTaking] = useState<string | null>(null);
  const [answer, setAnswer] = useState<OrderAnswer | null>(null);
  // A clock the countdowns read, so they tick without anything being asked for again.
  const [now, setNow] = useState(() => Date.now());

  const load = useCallback(async () => {
    try {
      setMarkets(await venue.markets());
      setFailed(null);
    } catch (e) {
      setFailed(message(e));
    }
  }, [venue]);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    const tick = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(tick);
  }, []);

  // While a request's window is open, ask what has come in for it. It stops on its own when the window closes: the
  // venue will send no further quote, so there is nothing left to see.
  useEffect(() => {
    if (!rfq || !rfq.open) return;
    const again = setInterval(async () => {
      try {
        setRfq(await venue.rfq(rfq.requestId));
      } catch (e) {
        setFailed(message(e));
      }
    }, 2000);
    return () => clearInterval(again);
  }, [venue, rfq]);

  // One leg per event, and only markets still trading. A parlay cannot be a leg of another, so they are left out
  // rather than shown and refused.
  const events = useMemo(() => byEvent(offerable(markets ?? [])), [markets]);

  const chosen = [...picks.values()];
  const wanted = Number(quantity);
  const askable = quantityOk(wanted);
  const enoughLegs = legsOk(chosen.length);

  const pick = (eventId: string, market: Market, outcome: Outcome) => {
    setPicks((current) => {
      const next = new Map(current);
      const had = next.get(eventId);
      // Tapping the same side again takes the leg off; the other side replaces it.
      if (had?.instrumentId === market.instrumentId && had.outcome === outcome) next.delete(eventId);
      else next.set(eventId, { instrumentId: market.instrumentId, outcome, title: market.title, label: market.label });
      return next;
    });
  };

  const ask = async () => {
    setSending(true);
    setFailed(null);
    setNote(null);
    setAnswer(null);
    try {
      const opened = await venue.requestQuote(
        chosen.map(({ instrumentId, outcome }) => ({ instrumentId, outcome })),
        wanted,
      );
      setRfq({ ...opened, listed: opened.listed ?? false, open: true, quotes: [] });
      setNote(
        opened.responders === 0
          ? 'The request is open, but nobody is listening for it: no quote will come.'
          : `Asked ${opened.responders} responder${opened.responders === 1 ? '' : 's'}.`,
      );
    } catch (e) {
      setFailed(message(e));
    } finally {
      setSending(false);
    }
  };

  // Taking a quote is an ordinary buy of YES on the parlay at the quote's price. IOC because the quote may already
  // have gone: fill against what is there and cancel the rest, rather than leaving an order resting on a parlay
  // nobody is quoting any more.
  const take = async (quote: Quote) => {
    setTaking(quote.orderId);
    setFailed(null);
    setAnswer(null);
    try {
      const placed = await venue.placeOrder({
        clientOrderId: `parlay-${Date.now().toString(36)}`,
        instrumentId: quote.instrumentId,
        action: 'buy',
        outcome: 'yes',
        price: { tick: quote.price.tick },
        quantity: quote.quantity,
        timeInForce: 'IOC',
      });
      setAnswer(placed);
      await refresh();
      if (rfq) setRfq(await venue.rfq(rfq.requestId));
    } catch (e) {
      setFailed(message(e));
    } finally {
      setTaking(null);
    }
  };

  const clear = () => {
    setPicks(new Map());
    setRfq(null);
    setNote(null);
    setAnswer(null);
  };

  return (
    <ScrollView
      contentContainerStyle={styles.page}
      refreshControl={<RefreshControl refreshing={false} onRefresh={() => void load()} />}>
      <Text style={styles.note}>
        A parlay pays $1 if every leg happens as you picked and nothing otherwise. Pick between {MIN_LEGS} and{' '}
        {MAX_LEGS} markets, one per event, then ask what it costs. Responders answer with firm quotes you can take or
        leave.
      </Text>

      {failed ? <Text style={styles.failed}>{failed}</Text> : null}
      {markets === null && !failed ? <ActivityIndicator /> : null}

      {events.length === 0 && markets !== null ? (
        <Text style={styles.note}>Nothing is trading to build a parlay from.</Text>
      ) : null}

      {events.map((group) => (
        <View key={group.eventId} style={styles.event}>
          <Text style={styles.heading}>{group.title}</Text>
          {group.markets.map((market) => {
            const held = picks.get(group.eventId);
            return (
              <View key={market.instrumentId} style={styles.row}>
                <Text style={styles.label}>{market.label}</Text>
                {(['yes', 'no'] as const).map((outcome) => {
                  const on = held?.instrumentId === market.instrumentId && held.outcome === outcome;
                  return (
                    <Pressable
                      key={outcome}
                      onPress={() => pick(group.eventId, market, outcome)}
                      accessibilityLabel={`${market.label} ${outcome}`}
                      style={[styles.choice, on && styles.chosen]}>
                      <Text>{outcome}</Text>
                    </Pressable>
                  );
                })}
              </View>
            );
          })}
        </View>
      ))}

      <View style={styles.ticket}>
        <Text style={styles.heading}>
          {chosen.length === 0 ? 'No legs picked' : `${chosen.length} leg${chosen.length === 1 ? '' : 's'}`}
        </Text>
        {chosen.map((leg) => (
          <Text key={leg.instrumentId} style={styles.note}>
            {leg.title} · {leg.label} {leg.outcome.toUpperCase()}
          </Text>
        ))}
        {!enoughLegs && chosen.length > 0 ? (
          <Text style={styles.note}>
            {chosen.length < MIN_LEGS
              ? `Pick at least ${MIN_LEGS}, each on a different event.`
              : `At most ${MAX_LEGS}.`}
          </Text>
        ) : null}

        <View style={styles.row}>
          <Text style={styles.label}>Contracts</Text>
          <TextInput
            value={quantity}
            onChangeText={setQuantity}
            keyboardType="number-pad"
            style={styles.quantity}
            accessibilityLabel="Contracts"
          />
        </View>
        {!askable ? <Text style={styles.note}>In multiples of {LOT_STEP}, at least {LOT_STEP}.</Text> : null}

        {!member ? (
          <Text style={styles.note}>Sign in on the account screen to ask for quotes.</Text>
        ) : (
          <View style={styles.row}>
            <Pressable
              disabled={!enoughLegs || !askable || sending}
              onPress={() => void ask()}
              style={[styles.action, (!enoughLegs || !askable || sending) && styles.disabled]}>
              <Text>{sending ? 'Asking…' : 'Request quotes'}</Text>
            </Pressable>
            <Pressable onPress={clear} style={styles.action}>
              <Text>Clear</Text>
            </Pressable>
          </View>
        )}
        {note ? <Text style={styles.note}>{note}</Text> : null}
      </View>

      {rfq ? <Request rfq={rfq} now={now} taking={taking} onTake={(q) => void take(q)} /> : null}

      {answer ? (
        <Text style={answer.status === 'rejected' ? styles.failed : styles.ok}>
          {answer.status === 'rejected'
            ? `Not bought: ${(answer.rejectReason ?? '').replaceAll('_', ' ').toLowerCase()}`
            : bought(answer)}
        </Text>
      ) : null}
    </ScrollView>
  );
}

function Request({
  rfq,
  now,
  taking,
  onTake,
}: {
  rfq: RfqStatus;
  now: number;
  taking: string | null;
  onTake: (quote: Quote) => void;
}) {
  const left = secondsUntil(rfq.expiresAt, now);
  const quotes = rfq.quotes ?? [];
  return (
    <View style={styles.ticket}>
      <Text style={styles.heading}>{rfq.open && left > 0 ? `Open for ${left}s` : 'The window has closed'}</Text>
      {quotes.length === 0 ? (
        <Text style={styles.note}>
          {rfq.open && left > 0 ? 'Waiting for a quote…' : 'Nobody quoted this one.'}
        </Text>
      ) : null}
      {quotes.map((quote) => {
        const lapses = quote.expiresAt ? `${secondsUntil(quote.expiresAt, now)}s` : 'until cancelled';
        const takeable = quote.state === 'open' && quote.quantity > 0;
        return (
          <View key={quote.orderId} style={styles.quote}>
            <View style={styles.row}>
              <Text style={styles.price}>{quote.price.cents}¢</Text>
              <Text style={styles.note}>
                {quote.quantity} left{quote.filled > 0 ? ` · ${quote.filled} taken` : ''} · {lapses}
              </Text>
            </View>
            {takeable ? (
              <Pressable
                disabled={taking !== null}
                onPress={() => onTake(quote)}
                accessibilityLabel={`Buy ${quote.quantity} at ${quote.price.cents} cents`}
                style={[styles.action, taking !== null && styles.disabled]}>
                <Text>
                  {taking === quote.orderId ? 'Buying…' : `Buy ${quote.quantity} at ${quote.price.cents}¢`}
                </Text>
              </Pressable>
            ) : (
              <Text style={styles.note}>{quote.state}</Text>
            )}
          </View>
        );
      })}
    </View>
  );
}

/** What an accepted quote actually came to. A partial fill is the ordinary case and not a failure: the quote may
 * have been taken in between, and only what was left of it is bought. */
function bought(answer: OrderAnswer): string {
  const filled = filledContracts(answer);
  if (filled === 0) return 'Nothing bought: the quote had already gone.';
  const at = answer.executions?.find((e) => e.type === 'fill')?.price;
  return (
    `Bought ${filled} contract${filled === 1 ? '' : 's'}${at ? ` at ${at.cents}¢` : ''}: ` +
    `pays ${money(payoutCents(filled))} if every leg happens.`
  );
}

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

const styles = StyleSheet.create({
  page: { padding: 16, gap: 16, maxWidth: 900, width: '100%', alignSelf: 'center' },
  event: { gap: 6 },
  heading: { fontWeight: '600' },
  label: { flex: 1 },
  row: { flexDirection: 'row', gap: 10, alignItems: 'center' },
  choice: {
    paddingVertical: 8,
    paddingHorizontal: 14,
    borderRadius: 8,
    borderWidth: StyleSheet.hairlineWidth,
    borderColor: '#8888',
  },
  chosen: { borderColor: '#2f6feb', borderWidth: 2 },
  ticket: { gap: 10, padding: 14, borderRadius: 10, borderWidth: StyleSheet.hairlineWidth, borderColor: '#8888' },
  quote: { gap: 8, paddingTop: 10, borderTopWidth: StyleSheet.hairlineWidth, borderTopColor: '#8888' },
  price: { fontWeight: '600', fontSize: 18 },
  quantity: { flex: 1, padding: 8, borderRadius: 8, borderWidth: StyleSheet.hairlineWidth, borderColor: '#8888' },
  action: {
    flex: 1,
    alignItems: 'center',
    padding: 12,
    borderRadius: 8,
    borderWidth: StyleSheet.hairlineWidth,
    borderColor: '#8888',
  },
  disabled: { opacity: 0.4 },
  note: { opacity: 0.6 },
  failed: { color: '#b00020' },
  ok: { color: '#1b7f3b' },
});
