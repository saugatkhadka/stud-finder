class Runner
  def run_everything(order)
    Refunder.new(order).lookup_order
    Refunder.new(order).charge_price
    Report.new.build_report(order)
    Report.new.summary_text(order)
    Dyn.new.call_it(order, :x)
    Dyn.new.quiet_method
    order.refund_order
    order.dup_name
    order.one_liner
    order.self_dispatch
  end
end
