class Report
  def build_report(record)
    record.line_total
  end

  def summary_text(record)
    record.shared_helper + record.abc
  end
end
